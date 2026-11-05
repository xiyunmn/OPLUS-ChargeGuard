//! Read-only charging telemetry. Power state is supplied by the event loop;
//! the five-second deadline only reads measurements and never wakes the device.
use crate::{config::Config, hardware::Hardware, storage, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const SAMPLE_MS: u64 = 5_000;
const GAP_MS: u64 = SAMPLE_MS * 2;
const INDEX_MAX: usize = 32 * 1024 * 1024;
static SERIAL: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default)]
pub struct ChargeState {
    pub connected: Option<bool>,
    pub status: String,
    pub protocol: Option<String>,
    pub source: Option<String>,
}

/// Units are established by the platform capability reader, never guessed here.
#[derive(Clone, Debug, Default)]
pub struct Readings {
    pub soc: Option<f64>,
    pub temp_c: Option<f64>,
    pub current_ma: Option<f64>,
    pub voltage_mv: Option<f64>,
    pub net_power_w: Option<f64>,
    pub protocol: Option<String>,
    pub current_raw: Option<String>,
    pub voltage_raw: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub seq: u64,
    pub unix_ms: u64,
    pub boottime_ms: u64,
    pub elapsed_ms: u64,
    pub soc: Option<f64>,
    pub temp_c: Option<f64>,
    pub current_ma: Option<f64>,
    pub voltage_mv: Option<f64>,
    pub net_power_w: Option<f64>,
    pub protocol: Option<String>,
    pub status: String,
    pub context_id: u64,
    pub current_raw: Option<String>,
    pub voltage_raw: Option<String>,
    pub gap_before: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub unix_ms: u64,
    pub boottime_ms: u64,
    pub elapsed_ms: u64,
    pub kind: String,
    pub detail: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Context {
    pub context_id: u64,
    pub unix_ms: u64,
    pub boottime_ms: u64,
    pub elapsed_ms: u64,
    pub revision: u64,
    pub config: Value,
    pub device: Value,
    pub measurement: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Record {
    Sample(Sample),
    Event(Event),
    Context(Context),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub started_unix_ms: u64,
    pub ended_unix_ms: Option<u64>,
    pub start_soc: Option<f64>,
    pub end_soc: Option<f64>,
    pub elapsed_ms: u64,
    pub charging_ms: u64,
    pub power_peak_w: Option<f64>,
    pub power_avg_w: Option<f64>,
    pub temp_max_c: Option<f64>,
    pub protocols: Vec<String>,
    pub complete: bool,
    pub end_reason: Option<String>,
    pub status: String,
    pub source: String,
    pub measurement: Value,
    pub latest: Option<Sample>,
    pub sparkline: Vec<Sample>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    pub from_ms: Option<u64>,
    pub to_ms: Option<u64>,
    pub protocols: Vec<String>,
    pub start_soc_max: Option<f64>,
    pub end_soc_min: Option<f64>,
    pub completeness: String,
}
impl Default for Filter {
    fn default() -> Self {
        Self {
            from_ms: None,
            to_ms: None,
            protocols: Vec::new(),
            start_soc_max: None,
            end_soc_min: None,
            completeness: "all".into(),
        }
    }
}
impl Filter {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.completeness.as_str(), "all" | "complete" | "partial")
            || self.from_ms.zip(self.to_ms).is_some_and(|(a, b)| a > b)
            || self.protocols.len() > 16
            || self.protocols.iter().any(|p| p.is_empty() || p.len() > 64)
            || [self.start_soc_max, self.end_soc_min]
                .iter()
                .flatten()
                .any(|v| !v.is_finite() || !(0.0..=100.0).contains(v))
        {
            return Err("invalid_statistics_filter".into());
        }
        Ok(())
    }
    fn matches(&self, s: &Summary) -> bool {
        self.from_ms.is_none_or(|v| s.started_unix_ms >= v)
            && self.to_ms.is_none_or(|v| s.started_unix_ms <= v)
            && (self.protocols.is_empty()
                || self.protocols.iter().any(|p| {
                    if p.eq_ignore_ascii_case("other") {
                        s.protocols.is_empty()
                            || s.protocols.iter().any(|q| {
                                !matches!(
                                    protocol_group(q).as_str(),
                                    "supervooc" | "ufcs" | "pps" | "pd"
                                )
                            })
                    } else {
                        s.protocols
                            .iter()
                            .any(|q| protocol_group(p) == protocol_group(q))
                    }
                }))
            && self
                .start_soc_max
                .is_none_or(|v| s.start_soc.is_some_and(|soc| soc <= v))
            && self
                .end_soc_min
                .is_none_or(|v| s.end_soc.is_some_and(|soc| soc >= v))
            && match self.completeness.as_str() {
                "complete" => s.complete,
                "partial" => !s.complete,
                _ => true,
            }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetailQuery {
    pub cursor: Option<u64>,
    pub from_elapsed_ms: Option<u64>,
    pub to_elapsed_ms: Option<u64>,
    pub max_points: usize,
}
#[derive(Serialize)]
pub struct Detail {
    pub summary: Summary,
    pub points: Vec<Sample>,
    pub events: Vec<Event>,
    pub cursor: u64,
    pub reset: bool,
}
#[derive(Default, Serialize)]
pub struct Aggregate {
    pub count: usize,
    pub charging_ms: u64,
    pub power_peak_w: Option<f64>,
    pub temp_max_c: Option<f64>,
}
#[derive(Serialize)]
pub struct List {
    pub active: Option<Summary>,
    pub latest: Option<Summary>,
    pub items: Vec<Summary>,
    pub total: usize,
    pub offset: usize,
    pub has_more: bool,
    pub aggregate: Aggregate,
    pub enabled: bool,
    pub retention_days: u32,
    pub measurement: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSource {
    pub summary: Summary,
    pub path: PathBuf,
    pub committed_bytes: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Session {
    summary: Summary,
    boot_id: String,
    started_boottime_ms: u64,
    last_unix_ms: u64,
    last_boottime_ms: u64,
    context_id: u64,
    revision: u64,
    observed_start: bool,
    interrupted: bool,
    state_since_boottime: u64,
    state_status: String,
    closed_charging_ms: u64,
    power_integral: f64,
    power_ms: u64,
    committed_bytes: u64,
}
impl Session {
    fn refresh_charging(&mut self, boottime_ms: u64) {
        self.summary.charging_ms = self.closed_charging_ms
            + if self.state_status == "Charging" {
                boottime_ms.saturating_sub(self.state_since_boottime)
            } else {
                0
            };
    }
    fn change_state(&mut self, boottime_ms: u64, status: &str) {
        self.refresh_charging(boottime_ms);
        self.closed_charging_ms = self.summary.charging_ms;
        self.state_since_boottime = boottime_ms;
        self.state_status = status.to_owned();
    }
    fn apply(&mut self, record: &Record) {
        let (unix, boot) = match record {
            Record::Sample(s) => (s.unix_ms, s.boottime_ms),
            Record::Event(e) => (e.unix_ms, e.boottime_ms),
            Record::Context(c) => (c.unix_ms, c.boottime_ms),
        };
        self.last_unix_ms = unix;
        self.last_boottime_ms = boot;
        match record {
            Record::Sample(s) => {
                self.refresh_charging(s.boottime_ms);
                if let Some(prev) = &self.summary.latest {
                    let dt = s.boottime_ms.saturating_sub(prev.boottime_ms);
                    if !s.gap_before && dt <= GAP_MS {
                        if let (Some(a), Some(b)) = (prev.net_power_w, s.net_power_w) {
                            self.power_integral += (a + b) * 0.5 * dt as f64;
                            self.power_ms += dt;
                        }
                    }
                }
                self.summary.start_soc = self.summary.start_soc.or(s.soc);
                self.summary.end_soc = s.soc.or(self.summary.end_soc);
                self.summary.elapsed_ms = s.elapsed_ms;
                self.summary.status = s.status.clone();
                self.summary.power_peak_w = maximum(self.summary.power_peak_w, s.net_power_w);
                self.summary.temp_max_c = maximum(self.summary.temp_max_c, s.temp_c);
                self.summary.power_avg_w =
                    (self.power_ms > 0).then(|| self.power_integral / self.power_ms as f64);
                if let Some(p) = &s.protocol {
                    if !self.summary.protocols.contains(p) {
                        self.summary.protocols.push(p.clone());
                    }
                }
                self.summary.latest = Some(s.clone());
                self.summary.sparkline.push(s.clone());
                if self.summary.sparkline.len() > 96 {
                    self.summary.sparkline = downsample(&self.summary.sparkline, 48, false);
                }
            }
            Record::Context(c) => {
                self.context_id = c.context_id;
                self.revision = c.revision;
                self.summary.measurement = c.measurement.clone();
            }
            Record::Event(e) => {
                self.summary.elapsed_ms = self.summary.elapsed_ms.max(e.elapsed_ms);
                self.refresh_charging(e.boottime_ms);
                if e.kind == "power_state" {
                    let status = e
                        .detail
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("Unknown");
                    self.change_state(e.boottime_ms, status);
                    self.summary.status = status.to_owned();
                }
                if e.kind == "data_gap" {
                    self.interrupted = true;
                }
                if e.kind == "session_end" {
                    self.change_state(e.boottime_ms, "Ended");
                    self.summary.ended_unix_ms = Some(e.unix_ms);
                    self.summary.end_reason = e
                        .detail
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    self.interrupted |= e
                        .detail
                        .get("interrupted")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    self.summary.complete = self.observed_start && !self.interrupted;
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}
impl Store {
    pub fn new(h: &Hardware) -> Result<Self> {
        let store = Self {
            dir: h.state().join("statistics"),
        };
        storage::secure_dir(&store.dir.join("sessions"))?;
        let recovery = {
            let _lock = store.lock()?;
            let index = store.dir.join("index.json");
            let active = store.dir.join("active.json");
            let indexed = index
                .exists()
                .then(|| store.closed())
                .transpose()
                .ok()
                .flatten();
            if indexed.is_none() || (active.exists() && store.active().is_err()) {
                Some((snapshot_stamp(&index)?, snapshot_stamp(&active)?, indexed))
            } else {
                None
            }
        };
        if let Some((index_stamp, active_stamp, indexed)) = recovery {
            // Recovery can scan long raw sessions; never hold the writer lock
            // while reading them. Do not publish over a concurrent snapshot.
            let recovered = store.recover(indexed.as_deref())?;
            let _lock = store.lock()?;
            let index = store.dir.join("index.json");
            let active = store.dir.join("active.json");
            if (index_stamp, active_stamp) != (snapshot_stamp(&index)?, snapshot_stamp(&active)?) {
                return Err("statistics_changed_during_recovery".into());
            }
            let mut pending = recovered
                .iter()
                .filter(|s| s.summary.ended_unix_ms.is_none());
            let current = pending.next();
            if pending.next().is_some() {
                return Err("multiple_unfinished_statistics".into());
            }
            store.save_closed(&recovered)?;
            if let Some(current) = current {
                snapshot_atomic(
                    &active,
                    &serde_json::to_vec(current).map_err(|e| e.to_string())?,
                )?;
            } else if active.exists() {
                regular(&active)?;
                fs::remove_file(&active).map_err(|e| e.to_string())?;
            }
        }
        {
            let _lock = store.lock()?;
            store.remove_orphans(&store.load()?)?;
        }
        Ok(store)
    }
    fn lock(&self) -> Result<storage::Lock> {
        storage::Lock::wait(&self.dir.join("store.lock"))
    }
    fn path(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty() || id.len() > 96 || !id.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
            return Err("invalid_statistics_id".into());
        }
        Ok(self.dir.join("sessions").join(format!("{id}.jsonl")))
    }
    fn closed(&self) -> Result<Vec<Session>> {
        let path = self.dir.join("index.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        storage::load(&path, INDEX_MAX)
    }
    fn active(&self) -> Result<Option<Session>> {
        let path = self.dir.join("active.json");
        if !path.exists() {
            return Ok(None);
        }
        storage::load(&path, 2 * 1024 * 1024).map(Some)
    }
    fn load(&self) -> Result<Vec<Session>> {
        let mut sessions = self.closed()?;
        if let Some(active) = self.active()? {
            // A completed index entry wins if termination was interrupted just
            // before removing its old active snapshot.
            if !sessions.iter().any(|s| s.summary.id == active.summary.id) {
                sessions.push(active);
            }
        }
        Ok(sessions)
    }
    fn recover(&self, indexed: Option<&[Session]>) -> Result<Vec<Session>> {
        // A valid index defines closed-session membership after deletion. Only
        // recover every raw session when that index itself is unavailable.
        let mut sessions = Vec::new();
        for entry in fs::read_dir(self.dir.join("sessions")).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let path = self.path(id)?;
            let file = open_regular(&path)?;
            let bound = file.metadata().map_err(|e| e.to_string())?.len();
            let mut reader = BufReader::new(file.take(bound));
            let mut line = String::new();
            let mut session: Option<Session> = None;
            let mut committed = 0;
            loop {
                line.clear();
                let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
                if n == 0 || !line.ends_with('\n') {
                    break;
                }
                let record: Record =
                    serde_json::from_str(&line).map_err(|_| "invalid_statistics_record")?;
                if session.is_none() {
                    if let Record::Event(e) = &record {
                        if e.kind == "session_start" {
                            session = Some(
                                serde_json::from_value(e.detail.clone())
                                    .map_err(|_| "invalid_statistics_start")?,
                            );
                        }
                    }
                }
                committed += n as u64;
                if let Some(s) = &mut session {
                    s.apply(&record);
                    s.committed_bytes = committed;
                }
            }
            if let Some(s) = session {
                if s.summary.ended_unix_ms.is_none()
                    || indexed.is_none_or(|closed| {
                        closed.iter().any(|old| old.summary.id == s.summary.id)
                    })
                {
                    sessions.push(s);
                }
            }
        }
        Ok(sessions)
    }
    fn remove_orphans(&self, sessions: &[Session]) -> Result<()> {
        for entry in fs::read_dir(self.dir.join("sessions")).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".jsonl") {
                if !sessions.iter().any(|s| s.summary.id == id) {
                    self.remove_raw(id)?;
                }
            }
        }
        Ok(())
    }
    fn save_closed(&self, sessions: &[Session]) -> Result<()> {
        let closed = sessions
            .iter()
            .filter(|s| s.summary.ended_unix_ms.is_some())
            .collect::<Vec<_>>();
        storage::atomic(
            &self.dir.join("index.json"),
            &serde_json::to_vec(&closed).map_err(|e| e.to_string())?,
        )
    }
    fn append(&self, session: &mut Session, records: &[Record]) -> Result<()> {
        let _lock = self.lock()?;
        let mut pending = session.clone();
        let path = self.path(&session.summary.id)?;
        regular(&path)?;
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| e.to_string())?;
        // A process interruption may leave an uncommitted tail. New writes are
        // only made by the recorder owning this session and its committed offset.
        file.set_len(session.committed_bytes)
            .map_err(|e| e.to_string())?;
        for record in records {
            let mut bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
            bytes.push(b'\n');
            file.write_all(&bytes).map_err(|e| e.to_string())?;
            pending.committed_bytes += bytes.len() as u64;
            pending.apply(record);
        }
        if pending.summary.ended_unix_ms.is_none() {
            snapshot_atomic(
                &self.dir.join("active.json"),
                &serde_json::to_vec(&pending).map_err(|e| e.to_string())?,
            )?;
        } else {
            // Recovery distinguishes closed deletion orphans from unfinished
            // recordings by their end row, which must precede durable membership.
            file.sync_all().map_err(|e| e.to_string())?;
            let mut sessions = self.closed()?;
            if let Some(old) = sessions
                .iter_mut()
                .find(|s| s.summary.id == pending.summary.id)
            {
                *old = pending.clone();
            } else {
                sessions.push(pending.clone());
            }
            self.save_closed(&sessions)?;
            if self
                .active()?
                .is_some_and(|s| s.summary.id == pending.summary.id)
            {
                let path = self.dir.join("active.json");
                regular(&path)?;
                fs::remove_file(path).map_err(|e| e.to_string())?;
            }
        }
        *session = pending;
        Ok(())
    }
    pub fn select(&self, filter: &Filter) -> Result<Vec<Summary>> {
        Ok(self
            .snapshot(filter)?
            .into_iter()
            .map(|s| s.summary)
            .collect())
    }
    pub fn snapshot(&self, filter: &Filter) -> Result<Vec<SessionSource>> {
        filter.validate()?;
        let _lock = self.lock()?;
        let mut out = self
            .load()?
            .into_iter()
            .filter(|s| filter.matches(&s.summary))
            .map(|s| {
                Ok(SessionSource {
                    path: self.path(&s.summary.id)?,
                    summary: s.summary,
                    committed_bytes: s.committed_bytes,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        out.sort_by(|a, b| {
            b.summary
                .started_unix_ms
                .cmp(&a.summary.started_unix_ms)
                .then_with(|| b.summary.id.cmp(&a.summary.id))
        });
        Ok(out)
    }
    pub fn open_source(&self, source: &SessionSource) -> Result<File> {
        if self.path(&source.summary.id)? != source.path {
            return Err("invalid_statistics_source".into());
        }
        let file = open_regular(&source.path)?;
        if file.metadata().map_err(|e| e.to_string())?.len() < source.committed_bytes {
            return Err("statistics_source_changed".into());
        }
        Ok(file)
    }
    pub fn record_snapshot(&self, id: &str) -> Result<(File, u64)> {
        let _lock = self.lock()?;
        let s = self
            .load()?
            .into_iter()
            .find(|s| s.summary.id == id)
            .ok_or("statistics_not_found")?;
        Ok((open_regular(&self.path(id)?)?, s.committed_bytes))
    }
    pub fn summary(&self, id: &str) -> Result<Summary> {
        self.path(id)?;
        let _lock = self.lock()?;
        self.load()?
            .into_iter()
            .find(|s| s.summary.id == id)
            .map(|s| s.summary)
            .ok_or_else(|| "statistics_not_found".into())
    }
    pub fn list(&self, filter: &Filter, offset: usize, c: &Config, h: &Hardware) -> Result<List> {
        filter.validate()?;
        let all = self.select(&Filter::default())?;
        let active = all.iter().find(|s| s.ended_unix_ms.is_none()).cloned();
        let latest = all.first().cloned();
        let selected = all
            .into_iter()
            .filter(|s| filter.matches(s))
            .collect::<Vec<_>>();
        let mut aggregate = Aggregate::default();
        for s in &selected {
            aggregate.count += 1;
            aggregate.charging_ms += s.charging_ms;
            aggregate.power_peak_w = maximum(aggregate.power_peak_w, s.power_peak_w);
            aggregate.temp_max_c = maximum(aggregate.temp_max_c, s.temp_max_c);
        }
        let total = selected.len();
        let items = selected
            .into_iter()
            .skip(offset)
            .take(20)
            .collect::<Vec<_>>();
        Ok(List {
            active,
            latest,
            has_more: offset.saturating_add(items.len()) < total,
            items,
            total,
            offset,
            aggregate,
            enabled: c.charge_statistics_enabled,
            retention_days: c.charge_statistics_retention_days,
            measurement: crate::runtime::statistics_measurement(h),
        })
    }
    pub fn detail(&self, id: &str, query: &DetailQuery) -> Result<Detail> {
        if query
            .from_elapsed_ms
            .zip(query.to_elapsed_ms)
            .is_some_and(|(a, b)| a > b)
            || query.max_points > 10_000
        {
            return Err("invalid_statistics_detail".into());
        }
        self.path(id)?;
        let (summary, mut file, end) = {
            let _lock = self.lock()?;
            let session = self
                .load()?
                .into_iter()
                .find(|s| s.summary.id == id)
                .ok_or("statistics_not_found")?;
            (
                session.summary,
                open_regular(&self.path(id)?)?,
                session.committed_bytes,
            )
        };
        let mut cursor = query.cursor.unwrap_or(0);
        let reset = cursor > end;
        if reset {
            cursor = 0;
        }
        if cursor > 0 {
            file.seek(SeekFrom::Start(cursor - 1))
                .map_err(|e| e.to_string())?;
            let mut byte = [0];
            file.read_exact(&mut byte).map_err(|e| e.to_string())?;
            if byte[0] != b'\n' {
                return Err("invalid_statistics_cursor".into());
            }
        }
        file.seek(SeekFrom::Start(cursor))
            .map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(file.take(end - cursor));
        let mut line = String::new();
        let max = if query.max_points == 0 {
            1200
        } else {
            query.max_points.max(24)
        };
        let mut points = Vec::new();
        let mut events = Vec::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            if !line.ends_with('\n') {
                return Err("incomplete_statistics_commit".into());
            }
            cursor += n as u64;
            match serde_json::from_str::<Record>(&line).map_err(|_| "invalid_statistics_record")? {
                Record::Sample(s)
                    if query.from_elapsed_ms.is_none_or(|v| s.elapsed_ms >= v)
                        && query.to_elapsed_ms.is_none_or(|v| s.elapsed_ms <= v) =>
                {
                    points.push(s);
                    if points.len() > max.saturating_mul(4) {
                        points = downsample(&points, max.saturating_mul(2), true);
                    }
                }
                Record::Event(e) => events.push(e),
                _ => (),
            }
        }
        Ok(Detail {
            summary,
            points: downsample(&points, max, true),
            events,
            cursor,
            reset,
        })
    }
    pub fn delete(&self, ids: &[String]) -> Result<usize> {
        if ids.len() > 1000 {
            return Err("too_many_statistics_ids".into());
        }
        for id in ids {
            self.path(id)?;
        }
        let _lock = self.lock()?;
        let mut sessions = self.load()?;
        if sessions
            .iter()
            .any(|s| ids.contains(&s.summary.id) && s.summary.ended_unix_ms.is_none())
        {
            return Err("cannot_delete_active_statistics".into());
        }
        let selected = sessions
            .iter()
            .filter(|s| ids.contains(&s.summary.id))
            .map(|s| s.summary.id.clone())
            .collect::<Vec<_>>();
        sessions.retain(|s| !selected.contains(&s.summary.id));
        self.commit_removal(&sessions, &selected)?;
        Ok(selected.len())
    }
    pub fn clear(&self) -> Result<usize> {
        let _lock = self.lock()?;
        let mut sessions = self.load()?;
        let ids = sessions
            .iter()
            .filter(|s| s.summary.ended_unix_ms.is_some())
            .map(|s| s.summary.id.clone())
            .collect::<Vec<_>>();
        sessions.retain(|s| s.summary.ended_unix_ms.is_none());
        self.commit_removal(&sessions, &ids)?;
        Ok(ids.len())
    }
    fn commit_removal(&self, sessions: &[Session], ids: &[String]) -> Result<()> {
        // A close interrupted after its index commit may leave an old active
        // snapshot. Remove it before the durable index commit also syncs this dir.
        if self.active()?.is_some_and(|s| ids.contains(&s.summary.id)) {
            let active = self.dir.join("active.json");
            regular(&active)?;
            fs::remove_file(active).map_err(|e| e.to_string())?;
        }
        self.save_closed(sessions)?;
        // A crash here leaves unreferenced raw files, never dangling index rows.
        for id in ids {
            self.remove_raw(id)?;
        }
        Ok(())
    }
    fn remove_raw(&self, id: &str) -> Result<()> {
        let path = self.path(id)?;
        regular(&path)?;
        fs::remove_file(path).map_err(|e| e.to_string())
    }
    pub fn retention(&self, days: u32) -> Result<()> {
        if !matches!(days, 30 | 90 | 180) {
            return Err("invalid_statistics_retention".into());
        }
        let cutoff = unix_ms().saturating_sub(days as u64 * 86_400_000);
        let _lock = self.lock()?;
        let mut sessions = self.load()?;
        let ids = sessions
            .iter()
            .filter(|s| s.summary.ended_unix_ms.is_some_and(|end| end < cutoff))
            .map(|s| s.summary.id.clone())
            .collect::<Vec<_>>();
        if !ids.is_empty() {
            sessions.retain(|s| !ids.contains(&s.summary.id));
            self.commit_removal(&sessions, &ids)?;
        }
        Ok(())
    }
    pub fn storage(&self) -> Result<Value> {
        let _lock = self.lock()?;
        let sessions = self.load()?;
        let mut raw_bytes = 0u64;
        for s in &sessions {
            raw_bytes += fs::metadata(self.path(&s.summary.id)?)
                .map_err(|e| e.to_string())?
                .len();
        }
        let index_bytes = fs::metadata(self.dir.join("index.json"))
            .map_err(|e| e.to_string())?
            .len()
            + fs::metadata(self.dir.join("active.json")).map_or(0, |m| m.len());
        Ok(
            json!({"bytes":raw_bytes+index_bytes,"raw_bytes":raw_bytes,"index_bytes":index_bytes,
            "sessions":sessions.len(),"active":sessions.iter().filter(|s| s.summary.ended_unix_ms.is_none()).count(),
            "oldest_unix_ms":sessions.iter().map(|s| s.summary.started_unix_ms).min()}),
        )
    }
}

pub struct Recorder {
    store: Store,
    active: Option<Session>,
    charge: ChargeState,
    revision: u64,
    enabled: bool,
    retention_days: u32,
    due_ms: Option<u64>,
    force_gap: bool,
}
impl Recorder {
    pub fn new(h: &Hardware, c: &Config, charge: ChargeState) -> Result<Self> {
        let store = Store::new(h)?;
        // A previous worker cannot provide an uninterrupted continuation. Replay
        // its complete tail, then end at the last observed time, never at reboot.
        let unfinished = {
            let _lock = store.lock()?;
            store
                .load()?
                .into_iter()
                .filter(|s| s.summary.ended_unix_ms.is_none())
                .collect::<Vec<_>>()
        };
        for mut session in unfinished {
            replay_tail(&store, &mut session)?;
            if session.summary.ended_unix_ms.is_some() {
                // The final row reached JSONL before a worker interruption, but
                // its index update did not. Preserve the proven normal ending.
                store.append(&mut session, &[])?;
                continue;
            }
            let (unix, boot) = (session.last_unix_ms, session.last_boottime_ms);
            let elapsed = session
                .summary
                .elapsed_ms
                .max(boot.saturating_sub(session.started_boottime_ms));
            let end = Record::Event(Event {
                unix_ms: unix,
                boottime_ms: boot,
                elapsed_ms: elapsed,
                kind: "session_end".into(),
                detail: json!({"reason":"interrupted","interrupted":true}),
            });
            store.append(&mut session, &[end])?;
        }
        store.retention(c.charge_statistics_retention_days)?;
        let mut recorder = Self {
            store,
            active: None,
            charge,
            revision: c.revision,
            enabled: c.charge_statistics_enabled,
            retention_days: c.charge_statistics_retention_days,
            due_ms: None,
            force_gap: false,
        };
        if recorder.enabled && recorder.charge.connected == Some(true) {
            recorder.start(h, c, false)?;
        }
        Ok(recorder)
    }
    pub fn update(
        &mut self,
        h: &Hardware,
        c: &Config,
        charge: ChargeState,
        power_event: bool,
    ) -> Result<()> {
        let old = self.charge.clone();
        let config_changed = c.revision != self.revision;
        if config_changed {
            self.store.retention(c.charge_statistics_retention_days)?;
            self.revision = c.revision;
            self.retention_days = c.charge_statistics_retention_days;
        }
        // The caller passes the last known state for configuration-only updates.
        self.charge = charge;
        if self.enabled && !c.charge_statistics_enabled {
            self.finish(h, "disabled")?;
        }
        self.enabled = c.charge_statistics_enabled;
        if !self.enabled {
            return Ok(());
        }
        if self.active.is_none() && self.charge.connected == Some(true) {
            self.start(h, c, power_event && old.connected == Some(false))?;
            return Ok(());
        }
        if self.active.is_none() {
            return Ok(());
        }
        if config_changed {
            self.context(h, c)?;
        }
        if !power_event {
            return Ok(());
        }
        if old.connected != self.charge.connected
            || old.status != self.charge.status
            || old.source != self.charge.source
        {
            let session = self.active.as_mut().unwrap();
            let boot = crate::runtime::now_ms();
            let event = Record::Event(Event {
                unix_ms: unix_ms(),
                boottime_ms: boot,
                elapsed_ms: boot.saturating_sub(session.started_boottime_ms),
                kind: "power_state".into(),
                detail: json!({"connected":self.charge.connected,"status":self.charge.status,"source":self.charge.source,"protocol":self.charge.protocol}),
            });
            self.store.append(session, &[event])?;
        }
        if self.charge.connected == Some(false) {
            self.due_ms = None;
            // Full already recorded its final point before periodic sampling
            // paused; unplugging later must not append a parked-interval tail.
            let measured = if old.status == "Full" {
                Ok(())
            } else {
                self.measure(h)
            };
            self.finish(h, "disconnected")?;
            measured?;
        } else if self.charge.connected.is_none() {
            self.finish(h, "power_state_unknown")?;
        } else if old.status != self.charge.status {
            self.force_gap |= old.status == "Full";
            self.due_ms =
                (self.charge.status != "Full").then(|| crate::runtime::now_ms() + SAMPLE_MS);
            self.measure(h)?;
        }
        Ok(())
    }
    fn start(&mut self, h: &Hardware, c: &Config, observed: bool) -> Result<()> {
        self.store.retention(self.retention_days)?;
        let boot = crate::runtime::now_ms();
        let unix = unix_ms();
        let id = format!(
            "{unix:013}-{boot:013}-{}",
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        let session = Session {
            summary: Summary {
                id,
                started_unix_ms: unix,
                ended_unix_ms: None,
                start_soc: None,
                end_soc: None,
                elapsed_ms: 0,
                charging_ms: 0,
                power_peak_w: None,
                power_avg_w: None,
                temp_max_c: None,
                protocols: Vec::new(),
                complete: false,
                end_reason: None,
                status: self.charge.status.clone(),
                source: self
                    .charge
                    .source
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                measurement: Value::Null,
                latest: None,
                sparkline: Vec::new(),
            },
            boot_id: h
                .read("/proc/sys/kernel/random/boot_id")
                .unwrap_or_default()
                .trim()
                .to_owned(),
            started_boottime_ms: boot,
            last_unix_ms: unix,
            last_boottime_ms: boot,
            context_id: 0,
            revision: c.revision,
            observed_start: observed,
            interrupted: false,
            state_since_boottime: boot,
            state_status: self.charge.status.clone(),
            closed_charging_ms: 0,
            power_integral: 0.0,
            power_ms: 0,
            committed_bytes: 0,
        };
        let start = Record::Event(Event {
            unix_ms: unix,
            boottime_ms: boot,
            elapsed_ms: 0,
            kind: "session_start".into(),
            detail: serde_json::to_value(&session).map_err(|e| e.to_string())?,
        });
        self.active = Some(session);
        self.store.append(self.active.as_mut().unwrap(), &[start])?;
        self.context(h, c)?;
        self.force_gap = false;
        self.due_ms = (self.charge.status != "Full").then(|| boot + SAMPLE_MS);
        self.measure(h)?;
        Ok(())
    }
    fn context(&mut self, h: &Hardware, c: &Config) -> Result<()> {
        let Some(session) = &mut self.active else {
            return Ok(());
        };
        let boot = crate::runtime::now_ms();
        let mut config = serde_json::to_value(c).map_err(|e| e.to_string())?;
        if let Some(object) = config.as_object_mut() {
            object.remove("revision");
            object.remove("schema");
            object.remove("detailed_logging");
            object.remove("charge_statistics_enabled");
            object.remove("charge_statistics_retention_days");
        }
        let unix = unix_ms();
        let context_id = session.context_id + 1;
        let changed = session.context_id > 0;
        let elapsed = boot.saturating_sub(session.started_boottime_ms);
        let context = Record::Context(Context {
            context_id,
            unix_ms: unix,
            boottime_ms: boot,
            elapsed_ms: boot.saturating_sub(session.started_boottime_ms),
            revision: c.revision,
            config,
            device: crate::runtime::statistics_context(h),
            measurement: crate::runtime::statistics_measurement(h),
        });
        let mut records = vec![context];
        if changed {
            records.push(Record::Event(Event {
                unix_ms: unix,
                boottime_ms: boot,
                elapsed_ms: elapsed,
                kind: "config_changed".into(),
                detail: json!({"revision":c.revision,"context_id":context_id}),
            }));
        }
        self.store.append(session, &records)
    }
    fn measure(&mut self, h: &Hardware) -> Result<()> {
        let Some(session) = &mut self.active else {
            return Ok(());
        };
        let readings = crate::runtime::statistics_readings(h);
        let boot = crate::runtime::now_ms();
        let sample = Sample {
            seq: session.summary.latest.as_ref().map_or(1, |s| s.seq + 1),
            unix_ms: unix_ms(),
            boottime_ms: boot,
            elapsed_ms: boot.saturating_sub(session.started_boottime_ms),
            soc: finite(readings.soc),
            temp_c: finite(readings.temp_c),
            current_ma: finite(readings.current_ma),
            voltage_mv: finite(readings.voltage_mv),
            net_power_w: finite(readings.net_power_w),
            protocol: readings.protocol,
            status: self.charge.status.clone(),
            context_id: session.context_id,
            current_raw: readings.current_raw,
            voltage_raw: readings.voltage_raw,
            gap_before: self.force_gap
                || session
                    .summary
                    .latest
                    .as_ref()
                    .is_some_and(|s| boot.saturating_sub(s.boottime_ms) > GAP_MS),
        };
        let mut records = Vec::new();
        if let Some(previous) = &session.summary.latest {
            let duration = sample.boottime_ms.saturating_sub(previous.boottime_ms);
            // Full intentionally has no periodic measurement. Its parked interval
            // breaks the chart, but is not missing telemetry during active sampling.
            if duration > GAP_MS && previous.status != "Full" {
                records.push(Record::Event(Event {
                    unix_ms: sample.unix_ms,
                    boottime_ms: boot,
                    elapsed_ms: sample.elapsed_ms,
                    kind: "data_gap".into(),
                    detail: json!({"from_elapsed_ms":previous.elapsed_ms,
                        "to_elapsed_ms":sample.elapsed_ms,"duration_ms":duration}),
                }));
            }
            if previous.protocol != sample.protocol {
                records.push(Record::Event(Event {
                    unix_ms: sample.unix_ms,
                    boottime_ms: boot,
                    elapsed_ms: sample.elapsed_ms,
                    kind: "protocol_changed".into(),
                    detail: json!({"from":previous.protocol,"to":sample.protocol}),
                }));
            }
        }
        records.push(Record::Sample(sample));
        self.store.append(session, &records)?;
        self.force_gap = false;
        Ok(())
    }
    pub fn sample_due(&mut self, h: &Hardware) -> Result<()> {
        let now = crate::runtime::now_ms();
        if self.due_ms.is_some_and(|due| now >= due) {
            // A failed append must not turn an expired deadline into a busy loop.
            self.due_ms = Some(now + SAMPLE_MS);
            self.measure(h)?;
        }
        Ok(())
    }
    pub fn next_due(&self) -> Option<Duration> {
        self.due_ms
            .map(|due| Duration::from_millis(due.saturating_sub(crate::runtime::now_ms())))
    }
    pub fn finish(&mut self, _h: &Hardware, reason: &str) -> Result<()> {
        self.due_ms = None;
        let Some(mut session) = self.active.take() else {
            return Ok(());
        };
        let boot = crate::runtime::now_ms();
        let end = Record::Event(Event {
            unix_ms: unix_ms(),
            boottime_ms: boot,
            elapsed_ms: boot.saturating_sub(session.started_boottime_ms),
            kind: "session_end".into(),
            detail: json!({"reason":reason,"interrupted":reason!="disconnected"}),
        });
        if let Err(e) = self.store.append(&mut session, &[end]) {
            self.active = Some(session);
            return Err(e);
        }
        Ok(())
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite())
}
fn maximum(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}
fn protocol_group(protocol: &str) -> String {
    match protocol.to_ascii_lowercase().as_str() {
        "svooc" | "supervooc" => "supervooc".into(),
        other => other.to_owned(),
    }
}

fn replay_tail(store: &Store, session: &mut Session) -> Result<()> {
    let mut file = open_regular(&store.path(&session.summary.id)?)?;
    file.seek(SeekFrom::Start(session.committed_bytes))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 || !line.ends_with('\n') {
            break;
        }
        let record =
            serde_json::from_str::<Record>(&line).map_err(|_| "invalid_statistics_record")?;
        session.apply(&record);
        session.committed_bytes += n as u64;
    }
    Ok(())
}

/// Retain extrema for every plotted series and both sides of explicit gaps.
/// Tiny sparklines retain power/temperature min/max pairs; detail retains all series.
fn downsample(points: &[Sample], max: usize, preserve_gaps: bool) -> Vec<Sample> {
    if points.len() <= max {
        return points.to_vec();
    }
    let series = if max <= 96 { 2 } else { 6 };
    let buckets = (max.saturating_sub(2) / (series * 2)).max(1);
    let size = points.len().div_ceil(buckets);
    let mut keep = BTreeSet::from([0, points.len() - 1]);
    for (i, p) in points.iter().enumerate() {
        if preserve_gaps && p.gap_before {
            keep.insert(i);
            if i > 0 {
                keep.insert(i - 1);
            }
        }
    }
    for begin in (0..points.len()).step_by(size) {
        let end = (begin + size).min(points.len());
        for metric in 0..series {
            let value = |s: &Sample| match metric {
                0 => s.net_power_w,
                1 => s.temp_c,
                2 => s.soc,
                3 => s.current_ma,
                4 => s.voltage_mv,
                _ => Some(s.elapsed_ms as f64),
            };
            let mut min = None::<(usize, f64)>;
            let mut max = None::<(usize, f64)>;
            for (i, p) in points.iter().enumerate().take(end).skip(begin) {
                if let Some(v) = value(p) {
                    if min.is_none_or(|(_, a)| v < a) {
                        min = Some((i, v));
                    }
                    if max.is_none_or(|(_, a)| v > a) {
                        max = Some((i, v));
                    }
                }
            }
            if let Some((i, _)) = min {
                keep.insert(i);
            }
            if let Some((i, _)) = max {
                keep.insert(i);
            }
        }
    }
    let mut previous = 0;
    keep.into_iter()
        .map(|i| {
            let mut point = points[i].clone();
            if !preserve_gaps && i > previous {
                point.gap_before |= points[previous + 1..=i].iter().any(|s| s.gap_before);
            }
            previous = i;
            point
        })
        .collect()
}

fn regular(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m)
            if m.is_file()
                && !m.file_type().is_symlink()
                && m.nlink() == 1
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o022 == 0 =>
        {
            Ok(())
        }
        Ok(_) => Err("unsafe_statistics_file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
fn snapshot_stamp(path: &Path) -> Result<Option<(u64, u64, i64, i64)>> {
    regular(path)?;
    match fs::metadata(path) {
        Ok(m) => Ok(Some((m.ino(), m.len(), m.mtime(), m.mtime_nsec()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
fn open_regular(path: &Path) -> Result<File> {
    regular(path)?;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| e.to_string())
}
/// Active measurement snapshots avoid a flash sync every five seconds. Closed
/// membership uses storage::atomic instead, so deletions survive a lost snapshot.
fn snapshot_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("missing_parent")?;
    storage::secure_dir(parent)?;
    regular(path)?;
    let temp = parent.join(format!(
        ".index-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        fs::rename(&temp, path).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_deleted_raw_does_not_reappear_after_interrupted_cleanup() {
        let root = std::env::temp_dir().join(format!(
            "charge-guard-delete-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let h = Hardware::fixture(root.clone());
        let mut recorder = Recorder::new(
            &h,
            &Config::default(),
            ChargeState {
                connected: Some(true),
                status: "Charging".into(),
                ..ChargeState::default()
            },
        )
        .unwrap();
        recorder.finish(&h, "disconnected").unwrap();
        let store = &recorder.store;
        let id = store.closed().unwrap()[0].summary.id.clone();
        let path = store.path(&id).unwrap();
        let raw = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        // Force raw removal to fail: its index membership must already be gone.
        assert_eq!(store.delete(&[id]).unwrap_err(), "unsafe_statistics_file");
        assert!(store.closed().unwrap().is_empty());
        fs::remove_dir(&path).unwrap();
        storage::atomic(&path, &raw).unwrap();
        // The next startup must honor the committed deletion even when another
        // snapshot is corrupt and forces JSONL recovery before orphan cleanup.
        storage::atomic(&store.dir.join("active.json"), b"{").unwrap();
        let recovered = Store::new(&h).unwrap();
        assert!(recovered.load().unwrap().is_empty());
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn statistics_boottime_gaps_and_signed_average() {
        let mut session = Session {
            summary: Summary {
                id: "1-1-1".into(),
                started_unix_ms: 10_000,
                ended_unix_ms: None,
                start_soc: None,
                end_soc: None,
                elapsed_ms: 0,
                charging_ms: 0,
                power_peak_w: None,
                power_avg_w: None,
                temp_max_c: None,
                protocols: Vec::new(),
                complete: false,
                end_reason: None,
                status: "Charging".into(),
                source: "usb".into(),
                measurement: Value::Null,
                latest: None,
                sparkline: Vec::new(),
            },
            boot_id: "boot".into(),
            started_boottime_ms: 1_000,
            last_unix_ms: 10_000,
            last_boottime_ms: 1_000,
            context_id: 1,
            revision: 1,
            observed_start: true,
            interrupted: false,
            state_since_boottime: 1_000,
            state_status: "Charging".into(),
            closed_charging_ms: 0,
            power_integral: 0.0,
            power_ms: 0,
            committed_bytes: 0,
        };
        let make = |seq, boot, unix, power, temp, gap, status: &str| Sample {
            seq,
            unix_ms: unix,
            boottime_ms: boot,
            elapsed_ms: boot - 1_000,
            soc: Some(seq as f64 * 10.0),
            temp_c: Some(temp),
            current_ma: None,
            voltage_mv: None,
            net_power_w: Some(power),
            protocol: Some("pps".into()),
            status: status.into(),
            context_id: 1,
            current_raw: None,
            voltage_raw: None,
            gap_before: gap,
        };
        for sample in [
            make(1, 1_000, 10_000, -1.0, 35.0, false, "Charging"),
            make(2, 6_000, 5_000, 3.0, 41.0, false, "Charging"),
            make(3, 26_000, 25_000, 99.0, 38.0, true, "Charging"),
            make(4, 31_000, 30_000, 5.0, 39.0, false, "Full"),
        ] {
            if sample.gap_before {
                session.apply(&Record::Event(Event { unix_ms:sample.unix_ms,boottime_ms:sample.boottime_ms,
                    elapsed_ms:sample.elapsed_ms,kind:"data_gap".into(),
                    detail:json!({"from_elapsed_ms":5_000,"to_elapsed_ms":25_000,"duration_ms":20_000}) }));
            }
            if sample.status == "Full" {
                session.apply(&Record::Event(Event {
                    unix_ms: sample.unix_ms,
                    boottime_ms: sample.boottime_ms,
                    elapsed_ms: sample.elapsed_ms,
                    kind: "power_state".into(),
                    detail: json!({"status":"Full"}),
                }));
            }
            session.apply(&Record::Sample(sample));
        }
        assert_eq!(session.summary.elapsed_ms, 30_000);
        assert_eq!(session.summary.charging_ms, 30_000);
        assert_eq!(session.summary.power_avg_w, Some(26.5));
        assert_eq!(session.summary.power_peak_w, Some(99.0));
        assert_eq!(session.summary.temp_max_c, Some(41.0));
        session.apply(&Record::Event(Event {
            unix_ms: 90_000,
            boottime_ms: 91_000,
            elapsed_ms: 90_000,
            kind: "session_end".into(),
            detail: json!({"reason":"disconnected","interrupted":false}),
        }));
        assert_eq!(session.summary.charging_ms, 30_000);
        assert!(!session.summary.complete);
        let points = (1..=150)
            .map(|i| {
                make(
                    i,
                    i * 5_000,
                    i * 5_000,
                    if i == 43 { 80.0 } else { 1.0 },
                    if i == 74 { 48.0 } else { 30.0 },
                    i == 93,
                    "Charging",
                )
            })
            .collect::<Vec<_>>();
        let spark = downsample(&points, 48, false);
        assert!(spark.iter().any(|s| s.net_power_w == Some(80.0)));
        assert!(spark.iter().any(|s| s.temp_c == Some(48.0)));
        assert!(spark.iter().any(|s| s.gap_before));
        let detail = downsample(&points, 48, true);
        assert!(detail.iter().any(|s| s.seq == 92));
        assert!(detail.iter().any(|s| s.seq == 93 && s.gap_before));
    }
}
