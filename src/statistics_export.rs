//! Detached, bounded-memory exports of a fixed charging-history snapshot.
use crate::{
    hardware::Hardware,
    runtime,
    statistics::{Filter, SessionSource, Store},
    storage, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

const PLAN_LIMIT: usize = 32 * 1024 * 1024;
const RECORD_LIMIT: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct Plan {
    cutoff_unix_ms: u64,
    request: Value,
    sources: Vec<SessionSource>,
}

#[derive(Serialize, Deserialize)]
struct Job {
    job_id: String,
    state: String,
    processed: u64,
    total: u64,
    processed_bytes: u64,
    total_bytes: u64,
    phase: String,
    owner: Option<runtime::Identity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn directory(h: &Hardware) -> PathBuf {
    h.state().join("statistics/exports")
}

fn valid_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 96
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("invalid_statistics_export_id".into());
    }
    Ok(())
}

fn job_path(h: &Hardware, id: &str) -> Result<PathBuf> {
    valid_id(id)?;
    Ok(directory(h).join(format!("{id}.json")))
}

fn plan_path(h: &Hardware, id: &str) -> Result<PathBuf> {
    valid_id(id)?;
    Ok(directory(h).join(format!("{id}.plan.json")))
}

// Only metadata operations hold this lock. The child waits until its parent has
// saved the spawned process identity, preventing queued/running write races.
fn metadata_lock(h: &Hardware) -> Result<File> {
    let dir = directory(h);
    storage::secure_dir(&dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("export.lock"))
        .map_err(|e| e.to_string())?;
    let m = file.metadata().map_err(|e| e.to_string())?;
    if !m.is_file()
        || m.nlink() != 1
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o022 != 0
    {
        return Err("unsafe_export_lock".into());
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(file)
}

fn save(h: &Hardware, job: &Job) -> Result<()> {
    storage::json(&job_path(h, &job.job_id)?, job)
}

fn reap_stale(h: &Hardware, job: &mut Job) -> Result<()> {
    if matches!(job.state.as_str(), "queued" | "running")
        && !job.owner.as_ref().is_some_and(runtime::alive)
    {
        job.state = "failed".into();
        job.error = Some("statistics_export_worker_exited".into());
        let _ = fs::remove_file(directory(h).join(format!("{}.zip.part", job.job_id)));
        save(h, job)?;
    }
    Ok(())
}

pub fn start(h: &Hardware, request: &Value) -> Result<Value> {
    let _lock = metadata_lock(h)?;
    let latest = directory(h).join("latest.json");
    let previous = if latest.exists() {
        let id: String = storage::load(&latest, 256)?;
        let mut job: Job = storage::load(&job_path(h, &id)?, 8192)?;
        reap_stale(h, &mut job)?;
        if matches!(job.state.as_str(), "queued" | "running") {
            return Err("statistics_export_already_running".into());
        }
        Some(id)
    } else {
        None
    };

    if !request.is_object() {
        return Err("invalid_statistics_export_request".into());
    }
    let filter: Filter =
        serde_json::from_value(request.get("filter").cloned().unwrap_or_else(|| json!({})))
            .map_err(|e| e.to_string())?;
    let mut ids = BTreeSet::new();
    if let Some(value) = request.get("ids") {
        let values = value.as_array().ok_or("invalid_statistics_session_ids")?;
        if values.is_empty() || values.len() > 10_000 {
            return Err("invalid_statistics_session_ids".into());
        }
        for value in values {
            let id = value.as_str().ok_or("invalid_statistics_session_id")?;
            valid_id(id)?;
            ids.insert(id.to_owned());
        }
    }
    let mut sources = Store::new(h)?.snapshot(&filter)?;
    if !ids.is_empty() {
        sources.retain(|s| ids.contains(&s.summary.id));
        if ids
            .iter()
            .any(|id| !sources.iter().any(|s| &s.summary.id == id))
        {
            return Err("statistics_session_not_found_or_filtered".into());
        }
    }
    if sources.is_empty() {
        return Err("statistics_export_empty".into());
    }
    for source in &mut sources {
        // Chart previews are irrelevant to the dataset selection plan; raw
        // samples are streamed from the committed JSONL snapshot instead.
        source.summary.sparkline.clear();
        source.summary.latest = None;
    }
    let cutoff_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as u64;
    let id = format!(
        "{}-{}-{}",
        cutoff_unix_ms,
        runtime::now_ms(),
        std::process::id()
    );
    let total = sources.len() as u64;
    let total_bytes = sources.iter().map(|s| s.committed_bytes).sum::<u64>() * 3;
    let plan = Plan {
        cutoff_unix_ms,
        request: request.clone(),
        sources,
    };
    let plan_bytes = serde_json::to_vec(&plan).map_err(|e| e.to_string())?;
    if plan_bytes.len() > PLAN_LIMIT {
        return Err("statistics_export_selection_too_large".into());
    }
    storage::atomic(&plan_path(h, &id)?, &plan_bytes)?;
    let mut job = Job {
        job_id: id.clone(),
        state: "queued".into(),
        processed: 0,
        total,
        processed_bytes: 0,
        total_bytes,
        phase: "queued".into(),
        owner: None,
        path: None,
        public_path: None,
        public_error: None,
        bytes: None,
        error: None,
    };
    save(h, &job)?;
    storage::json(&latest, &id)?;
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command
        .args(["statistics-export-worker", &id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let spawned = command.spawn().map_err(|e| e.to_string());
    match spawned {
        Ok(child) => match runtime::process_identity(child.id()) {
            Ok(owner) => {
                job.owner = Some(owner);
                save(h, &job)?;
            }
            Err(error) => {
                job.state = "failed".into();
                job.error = Some(error);
                save(h, &job)?;
            }
        },
        Err(error) => {
            job.state = "failed".into();
            job.error = Some(error);
            save(h, &job)?;
        }
    }
    if let Some(previous) = previous {
        // User-created archives remain available; only the previous small job
        // record and selection plan are superseded.
        let _ = fs::remove_file(job_path(h, &previous)?);
        let _ = fs::remove_file(plan_path(h, &previous)?);
    }
    Ok(json!({"job_id":id}))
}

pub fn status(h: &Hardware, id: &str) -> Result<Value> {
    let _lock = metadata_lock(h)?;
    let mut job: Job = storage::load(&job_path(h, id)?, 8192)?;
    reap_stale(h, &mut job)?;
    public_status(&job)
}

fn public_status(job: &Job) -> Result<Value> {
    let mut value = serde_json::to_value(job).map_err(|e| e.to_string())?;
    value.as_object_mut().unwrap().remove("owner");
    Ok(value)
}

pub fn run(h: &Hardware, id: &str) -> Result<Value> {
    let mut job: Job;
    {
        let _lock = metadata_lock(h)?;
        job = storage::load(&job_path(h, id)?, 8192)?;
        if job.state != "queued" {
            return Err("statistics_export_not_queued".into());
        }
        job.owner = Some(runtime::process_identity(std::process::id())?);
        job.state = "running".into();
        job.phase = "sessions".into();
        save(h, &job)?;
    }
    let result = (|| {
        let plan: Plan = storage::load(&plan_path(h, id)?, PLAN_LIMIT)?;
        write_archive(h, &mut job, &plan)
    })();
    if let Err(error) = result {
        job.state = "failed".into();
        job.error = Some(error);
        let _ = fs::remove_file(directory(h).join(format!("{id}.zip.part")));
    }
    let _lock = metadata_lock(h)?;
    save(h, &job)?;
    public_status(&job)
}

fn csv_row(out: &mut impl Write, values: &[Value]) -> Result<()> {
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            out.write_all(b",").map_err(|e| e.to_string())?;
        }
        let text = match value {
            Value::Null => String::new(),
            Value::String(value) => value.clone(),
            value => value.to_string(),
        };
        if text
            .bytes()
            .any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r'))
        {
            out.write_all(b"\"").map_err(|e| e.to_string())?;
            out.write_all(text.replace('"', "\"\"").as_bytes())
                .map_err(|e| e.to_string())?;
            out.write_all(b"\"").map_err(|e| e.to_string())?;
        } else {
            out.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        }
    }
    out.write_all(b"\r\n").map_err(|e| e.to_string())
}

fn selected(value: &Value, fields: &[&str]) -> Vec<Value> {
    fields
        .iter()
        .map(|name| value.get(*name).cloned().unwrap_or(Value::Null))
        .collect()
}

fn entry(zip: &mut ZipWriter<File>, name: &str) -> Result<()> {
    zip.start_file(
        name,
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(1)),
    )
    .map_err(|e| e.to_string())
}

// Each pass owns at most one bounded JSONL record. Committed byte limits are
// captured at start and never replaced by the live file's current length.
fn records<F>(
    h: &Hardware,
    store: &Store,
    source: &SessionSource,
    job: &mut Job,
    mut consume: F,
) -> Result<()>
where
    F: FnMut(&str, &Value) -> Result<()>,
{
    let file = store.open_source(source)?;
    let mut reader = BufReader::new(file.take(source.committed_bytes));
    let mut line = Vec::new();
    let mut last_save = Instant::now();
    let mut consumed = 0;
    loop {
        line.clear();
        let n = (&mut reader)
            .take((RECORD_LIMIT + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if n > RECORD_LIMIT || line.last() != Some(&b'\n') {
            return Err("statistics_export_invalid_committed_record".into());
        }
        let value: Value =
            serde_json::from_slice(&line).map_err(|e| format!("statistics_export_record:{e}"))?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or("statistics_export_record_type")?;
        if !matches!(kind, "sample" | "event" | "context") {
            return Err("statistics_export_record_type".into());
        }
        let data = value.get("data").ok_or("statistics_export_record_data")?;
        consume(kind, data)?;
        consumed += n as u64;
        job.processed_bytes += n as u64;
        if last_save.elapsed().as_millis() >= 500 {
            save(h, job)?;
            last_save = Instant::now();
        }
    }
    if consumed != source.committed_bytes {
        return Err("statistics_export_source_truncated".into());
    }
    save(h, job)
}

fn write_archive(h: &Hardware, job: &mut Job, plan: &Plan) -> Result<()> {
    let store = Store::new(h)?;
    let temp = directory(h).join(format!("{}.zip.part", job.job_id));
    let filename = format!("{}-statistics-{}.zip", crate::ID, job.job_id);
    let path = directory(h).join(&filename);
    let file = OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    let mut zip = ZipWriter::new(file);
    let session_fields = [
        "id",
        "started_unix_ms",
        "ended_unix_ms",
        "start_soc",
        "end_soc",
        "elapsed_ms",
        "charging_ms",
        "power_peak_w",
        "power_avg_w",
        "temp_max_c",
        "protocols",
        "complete",
        "end_reason",
        "status",
        "source",
    ];
    entry(&mut zip, "sessions.csv")?;
    csv_row(
        &mut zip,
        &session_fields.iter().map(|s| json!(s)).collect::<Vec<_>>(),
    )?;
    for source in &plan.sources {
        let summary = serde_json::to_value(&source.summary).map_err(|e| e.to_string())?;
        csv_row(&mut zip, &selected(&summary, &session_fields))?;
    }
    let sample_fields = [
        "seq",
        "unix_ms",
        "boottime_ms",
        "elapsed_ms",
        "soc",
        "temp_c",
        "current_ma",
        "voltage_mv",
        "net_power_w",
        "protocol",
        "status",
        "context_id",
        "current_raw",
        "voltage_raw",
        "gap_before",
    ];
    entry(&mut zip, "samples.csv")?;
    let mut header = vec![json!("session_id")];
    header.extend(sample_fields.iter().map(|s| json!(s)));
    csv_row(&mut zip, &header)?;
    job.phase = "samples".into();
    job.processed = 0;
    let mut sample_count = 0u64;
    for source in &plan.sources {
        records(h, &store, source, job, |kind, data| {
            if kind == "sample" {
                let mut row = vec![json!(source.summary.id)];
                row.extend(selected(data, &sample_fields));
                csv_row(&mut zip, &row)?;
                sample_count += 1;
            }
            Ok(())
        })?;
        job.processed += 1;
        save(h, job)?;
    }
    entry(&mut zip, "events.csv")?;
    let event_fields = ["unix_ms", "boottime_ms", "elapsed_ms", "kind", "detail"];
    let mut header = vec![json!("session_id")];
    header.extend(event_fields.iter().map(|s| json!(s)));
    csv_row(&mut zip, &header)?;
    job.phase = "events".into();
    job.processed = 0;
    let mut event_count = 0u64;
    for source in &plan.sources {
        records(h, &store, source, job, |kind, data| {
            if kind == "event" {
                let mut row = vec![json!(source.summary.id)];
                row.extend(selected(data, &event_fields));
                csv_row(&mut zip, &row)?;
                event_count += 1;
            }
            Ok(())
        })?;
        job.processed += 1;
        save(h, job)?;
    }
    entry(&mut zip, "context.json")?;
    let mut metadata = json!({"schema":1,"export_environment":{
        "device":{},"kernel":{"version":h.read("/proc/version").ok()},
        "module":{"id":crate::ID,"name":crate::NAME,"core_version":crate::VERSION},
        "configuration":runtime::config(h).ok(),
        "note":"Export-time metadata only; each recorded context supplies its historical device and measurement interpretation."
    }});
    for key in [
        "ro.product.model",
        "ro.product.brand",
        "ro.product.manufacturer",
        "ro.build.fingerprint",
        "ro.build.display.id",
        "ro.build.version.release",
        "ro.build.version.sdk",
        "ro.build.version.oplusrom",
    ] {
        metadata["export_environment"]["device"][key] = json!(h.prop(key).ok());
    }
    if let Ok(props) = h.read(&format!("{}/module.prop", crate::MODULE)) {
        for line in props.lines() {
            if let Some((key, value)) = line.split_once('=') {
                if matches!(key, "version" | "versionCode") {
                    metadata["export_environment"]["module"][key] = json!(value);
                }
            }
        }
    }
    let mut prefix = serde_json::to_vec(&metadata).map_err(|e| e.to_string())?;
    prefix.pop();
    zip.write_all(&prefix).map_err(|e| e.to_string())?;
    zip.write_all(b",\"sessions\":[")
        .map_err(|e| e.to_string())?;
    job.phase = "context".into();
    job.processed = 0;
    let mut context_count = 0u64;
    for (index, source) in plan.sources.iter().enumerate() {
        if index != 0 {
            zip.write_all(b",").map_err(|e| e.to_string())?;
        }
        zip.write_all(b"{\"session_id\":")
            .map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut zip, &source.summary.id).map_err(|e| e.to_string())?;
        zip.write_all(b",\"contexts\":[")
            .map_err(|e| e.to_string())?;
        let mut first = true;
        records(h, &store, source, job, |kind, data| {
            if kind == "context" {
                if !first {
                    zip.write_all(b",").map_err(|e| e.to_string())?;
                }
                serde_json::to_writer(&mut zip, data).map_err(|e| e.to_string())?;
                first = false;
                context_count += 1;
            }
            Ok(())
        })?;
        job.processed += 1;
        save(h, job)?;
        zip.write_all(b"]}").map_err(|e| e.to_string())?;
    }
    zip.write_all(b"]}").map_err(|e| e.to_string())?;
    entry(&mut zip, "manifest.json")?;
    let manifest = json!({
        "schema":1,"format":"charge_guard_statistics","encoding":"UTF-8","csv_line_ending":"CRLF",
        "cutoff_unix_ms":plan.cutoff_unix_ms,"request":plan.request,
        "sessions":plan.sources.len(),"samples":sample_count,"events":event_count,"contexts":context_count,
        "snapshot":plan.sources.iter().map(|s| json!({"session_id":s.summary.id,"committed_bytes":s.committed_bytes,"partial":!s.summary.complete})).collect::<Vec<_>>(),
        "sampling":{"nominal_interval_ms":5000,"export_downsampled":false},
        "units":{"unix_ms":"milliseconds since Unix epoch","boottime_ms":"milliseconds since boot including suspend","elapsed_ms":"milliseconds since session start","soc":"percent","temp_c":"degree Celsius","current_ma":"mA, positive charging","voltage_mv":"mV, representative single-cell voltage","net_power_w":"W, positive charging"},
        "measurement_method":"For each sample join session_id/context_id to context.json. Its recorded measurement describes raw units, polarity, cell count and net-power calculation. Unknown conversion or topology leaves normalized readings unavailable. Raw values preserve the original driver sign and text. Battery net power is not adapter input power or an efficiency measurement.",
        "missing_values":"Empty CSV fields and JSON null mean unavailable; never zero-filled.",
        "time_method":"Ordering and durations use boottime/elapsed time. Wall-clock timestamps may jump; sessions may span clock corrections.",
        "privacy":"Only listed device/build properties, module metadata and configuration contexts; no serial, application list, getprop dump or diagnostic logs."
    });
    serde_json::to_writer_pretty(&mut zip, &manifest).map_err(|e| e.to_string())?;
    entry(&mut zip, "README.txt")?;
    zip.write_all(README.as_bytes())
        .map_err(|e| e.to_string())?;
    let archive = zip.finish().map_err(|e| e.to_string())?;
    archive.sync_all().map_err(|e| e.to_string())?;
    let bytes = archive.metadata().map_err(|e| e.to_string())?.len();
    drop(archive);
    fs::rename(&temp, &path).map_err(|e| e.to_string())?;
    job.path = Some(path.clone());
    job.bytes = Some(bytes);
    job.phase = "publishing".into();
    save(h, job)?;
    match publish(h, &path, &filename) {
        Ok(public_path) => job.public_path = Some(public_path),
        Err(error) => job.public_error = Some(error),
    }
    job.phase = "complete".into();
    job.state = "complete".into();
    Ok(())
}

fn publish(h: &Hardware, source: &Path, filename: &str) -> Result<PathBuf> {
    if !h.exists("/sdcard/Download") {
        return Err("Download directory unavailable".into());
    }
    let path = h.path(&format!("/sdcard/Download/{filename}"));
    let mut created = false;
    let result = (|| {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| e.to_string())?;
        created = true;
        let mut input = File::open(source).map_err(|e| e.to_string())?;
        std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        output.sync_all().map_err(|e| e.to_string())
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(&path);
    }
    result.map(|_| path)
}

const README: &str = "Charge Guard charging statistics dataset\r\n\r\n\
sessions.csv: one frozen summary per selected session. protocols is a JSON array.\r\n\
samples.csv: every committed raw sample; no chart downsampling. seq is scoped to a session.\r\n\
events.csv: session events; detail is JSON.\r\n\
context.json: whitelisted device/build metadata and the configuration revision contexts referenced by samples.\r\n\
manifest.json: fixed cutoff, committed byte bounds, selection, row counts, units and calculation methods.\r\n\r\n\
CSV uses UTF-8, English stable column names, comma delimiters, RFC 4180 quoting and CRLF line endings.\r\n\
Missing numeric readings are empty fields, never fabricated zeros. JSON uses null for missing values.\r\n\
current_ma is mA and net_power_w is W, normalized positive for charging. voltage_mv is representative single-cell voltage in mV.\r\n\
current_raw and voltage_raw preserve the original driver text and sign. Their units must be read from the sample's recorded measurement context; do not assume microampere.\r\n\
Join session_id/context_id to context.json for historical device/build, raw-unit conversion, polarity, cell count and the net-power calculation. Unknown conversions/topology yield empty normalized values.\r\n\
For a confirmed single-cell representative voltage, battery net power uses current_ma * voltage_mv * cell_count / 1000000. This is not adapter input power.\r\n\
Nominal sampling interval is 5000 ms; do not assume uniform spacing. Gaps, suspend and clock corrections can occur. Use boottime/elapsed_ms for durations.\r\n\
An active session is exported as a partial session at the frozen committed cutoff. Later samples are excluded.\r\n\
Configuration contexts contain recorded revisions, measurement interpretation and historical device metadata. export_environment describes the export-time environment only and never replaces historical contexts.\r\n\
The archive excludes serial numbers, application lists, full property dumps and diagnostic logs.\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_dataset_keeps_raw_samples_and_cutoff() {
        let root = std::env::temp_dir().join(format!(
            "charge-guard-export-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let h = Hardware::fixture(root.clone());
        let store = Store::new(&h).unwrap();
        storage::secure_dir(&directory(&h)).unwrap();
        storage::atomic(&h.path("/properties/ro.build.display.id"), b"new-ROM").unwrap();
        let measurement = json!({"raw_current_unit":"mA","raw_voltage_unit":"uV","cell_count":2,"polarity":"negative_charging"});
        let context = json!({"type":"context","data":{"context_id":7,"unix_ms":1000,"boottime_ms":1000,
            "elapsed_ms":0,"revision":3,"config":{"revision":3},"device":{"build":"old-ROM"},"measurement":measurement}});
        let sample = |seq: u64| {
            json!({"type":"sample","data":{"seq":seq,"unix_ms":1000+seq*5000,
            "boottime_ms":1000+seq*5000,"elapsed_ms":seq*5000,"soc":if seq==2 {Value::Null} else {json!(50)},
            "temp_c":31,"current_ma":seq*100,"voltage_mv":4000,"net_power_w":seq as f64*0.8,
            "protocol":"PPS","status":"Charging","context_id":7,"current_raw":format!("-{}",seq*100),
            "voltage_raw":"4000000","gap_before":false}})
        };
        let encode = |records: Vec<Value>| {
            records
                .into_iter()
                .map(|v| format!("{}\n", v))
                .collect::<String>()
        };
        let raw = encode(vec![context.clone(), sample(1), sample(2), sample(3)]);
        let selected_path = h.state().join("statistics/sessions/1000-1.jsonl");
        let excluded_path = h.state().join("statistics/sessions/2000-1.jsonl");
        storage::atomic(&selected_path, raw.as_bytes()).unwrap();
        let excluded_raw = encode(vec![sample(99)]);
        storage::atomic(&excluded_path, excluded_raw.as_bytes()).unwrap();
        let summary = |id: &str, protocol: &str, ended: Value| {
            json!({"id":id,"started_unix_ms":1000,
            "ended_unix_ms":ended,"start_soc":50,"end_soc":51,"elapsed_ms":15000,"charging_ms":15000,
            "power_peak_w":2.4,"power_avg_w":1.6,"temp_max_c":31,"protocols":[protocol],"complete":false,
            "end_reason":null,"status":"Charging","source":"usb","measurement":measurement,
            "latest":sample(3)["data"],"sparkline":[sample(1)["data"]]})
        };
        let session = |summary: Value, bytes: usize| {
            json!({"summary":summary,"boot_id":"old-boot",
            "started_boottime_ms":1000,"last_unix_ms":16000,"last_boottime_ms":16000,
            "context_id":7,"revision":3,"observed_start":true,"interrupted":false,
            "state_since_boottime":1000,"state_status":"Charging","closed_charging_ms":0,
            "power_integral":0,"power_ms":0,"committed_bytes":bytes})
        };
        storage::json(
            &h.state().join("statistics/active.json"),
            &session(summary("1000-1", "PPS", Value::Null), raw.len()),
        )
        .unwrap();
        storage::json(
            &h.state().join("statistics/index.json"),
            &vec![session(
                summary("2000-1", "PD", json!(17000)),
                excluded_raw.len(),
            )],
        )
        .unwrap();
        let filter = Filter {
            protocols: vec!["PPS".into()],
            ..Filter::default()
        };
        let sources = store.snapshot(&filter).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].summary.sparkline.len(), 1);
        // The file continues to grow after selection. Neither these records nor
        // their newer interpretation may leak across the frozen committed bound.
        let mut later = context;
        later["data"]["context_id"] = json!(8);
        later["data"]["measurement"]["raw_current_unit"] = json!("uA");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&selected_path)
            .unwrap();
        file.write_all(encode(vec![later, sample(4)]).as_bytes())
            .unwrap();
        drop(file);
        let mut job = Job {
            job_id: "dataset-fixture".into(),
            state: "running".into(),
            processed: 0,
            total: 1,
            processed_bytes: 0,
            total_bytes: raw.len() as u64 * 3,
            phase: "samples".into(),
            owner: None,
            path: None,
            public_path: None,
            public_error: None,
            bytes: None,
            error: None,
        };
        let plan = Plan {
            cutoff_unix_ms: 16000,
            request: json!({"filter":filter}),
            sources,
        };
        write_archive(&h, &mut job, &plan).unwrap();
        assert!(job.public_path.is_none());
        assert!(job.public_error.is_some());
        let mut zip =
            zip::ZipArchive::new(File::open(job.path.as_ref().unwrap()).unwrap()).unwrap();
        fn member(zip: &mut zip::ZipArchive<File>, name: &str) -> String {
            let mut text = String::new();
            zip.by_name(name)
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            text
        }
        assert_eq!(zip.len(), 6);
        let csv = member(&mut zip, "samples.csv");
        let rows = csv
            .lines()
            .skip(1)
            .map(|line| line.split(',').collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().map(|r| r[1]).collect::<Vec<_>>(),
            vec!["1", "2", "3"]
        );
        assert!(rows.iter().all(|r| r[0] == "1000-1" && r[12] == "7"));
        assert_eq!(rows[1][5], "");
        assert_eq!(rows[1][13], "-200");
        let sessions = member(&mut zip, "sessions.csv");
        assert_eq!(sessions.lines().count(), 2);
        assert!(!sessions.contains("2000-1"));
        let contexts: Value = serde_json::from_str(&member(&mut zip, "context.json")).unwrap();
        let historical = &contexts["sessions"][0]["contexts"];
        assert_eq!(historical.as_array().unwrap().len(), 1);
        assert_eq!(historical[0]["measurement"], measurement);
        assert_eq!(historical[0]["device"]["build"], "old-ROM");
        assert_eq!(
            contexts["export_environment"]["device"]["ro.build.display.id"],
            "new-ROM"
        );
        let manifest: Value = serde_json::from_str(&member(&mut zip, "manifest.json")).unwrap();
        assert_eq!(manifest["samples"], 3);
        assert_eq!(manifest["cutoff_unix_ms"], 16000);
        assert_eq!(manifest["snapshot"][0]["committed_bytes"], raw.len());
        assert_eq!(manifest["snapshot"][0]["partial"], true);
        drop(zip);
        fs::remove_dir_all(root).unwrap();
    }
}
