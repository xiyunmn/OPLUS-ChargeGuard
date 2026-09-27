//! Firmware-pinned, reversible charge-only policy overlay. No charger node writes.
use crate::{command, hardware::Hardware, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    time::{Duration, Instant},
};

pub const ID: &str = "engineer_charge_policy";
pub const TARGET: &str = "/odm/etc/temperature_profile/sys_thermal_control_config.xml";
pub const SOURCE: &str = "/dev/charge_guard_masks/engineer_policy.xml";
pub const XML_HASH: &str = "fdc725560c4ac3f6c1a5adcecb2fd67ac924f1897eee4ec1eb85456a485ad105";
const COMPONENT: &str = "com.oplus.battery/com.oplus.battery.OplusBatteryService";
const MAX_XML: usize = 512 * 1024;

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn bytes(h: &Hardware, path: &str, max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    fs::File::open(h.path(path))
        .map_err(|e| e.to_string())?
        .take((max + 1) as u64)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    if out.len() > max {
        return Err("policy_input_too_large".into());
    }
    Ok(out)
}
fn hash(h: &Hardware, path: &str, max: usize) -> Result<String> {
    Ok(digest(&bytes(h, path, max)?))
}

#[derive(Debug)]
struct Tag {
    path: Vec<String>,
    attrs: BTreeMap<String, (String, std::ops::Range<usize>)>,
    insert: usize,
}
// A deliberately narrow XML lexer: preserve every original byte outside charge
// attributes, reject declarations/entities and malformed input rather than repair it.
fn tags(xml: &str) -> Result<Vec<Tag>> {
    if xml.len() > MAX_XML || xml.contains('&') {
        return Err("policy_xml_entities_or_size".into());
    }
    let b = xml.as_bytes();
    let mut pos = 0;
    let mut stack = Vec::<String>::new();
    let mut out = Vec::new();
    let mut roots = 0;
    while pos < b.len() {
        if b[pos] != b'<' {
            // Version/filter text is allowed inside the document only.
            if stack.is_empty() && !b[pos].is_ascii_whitespace() {
                return Err("policy_xml_text_outside_root".into());
            }
            pos += 1;
            continue;
        }
        if xml[pos..].starts_with("<!--") {
            pos += xml[pos + 4..].find("-->").ok_or("policy_xml_comment")? + 7;
            continue;
        }
        if xml[pos..].starts_with("<?xml ") && roots == 0 {
            pos += xml[pos + 2..].find("?>").ok_or("policy_xml_header")? + 4;
            continue;
        }
        pos += 1;
        let close = b.get(pos) == Some(&b'/');
        if close {
            pos += 1;
        }
        let start = pos;
        while pos < b.len() && (b[pos].is_ascii_alphanumeric() || b"_.:-".contains(&b[pos])) {
            pos += 1;
        }
        if pos == start {
            return Err("policy_xml_tag".into());
        }
        let name = xml[start..pos].to_owned();
        let mut attrs = BTreeMap::new();
        loop {
            while pos < b.len() && b[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if matches!(b.get(pos), Some(b'>') | Some(b'/')) {
                break;
            }
            let start = pos;
            while pos < b.len() && (b[pos].is_ascii_alphanumeric() || b"_.:-".contains(&b[pos])) {
                pos += 1;
            }
            if close || pos == start {
                return Err("policy_xml_attribute".into());
            }
            let key = xml[start..pos].to_owned();
            while pos < b.len() && b[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if b.get(pos) != Some(&b'=') {
                return Err("policy_xml_equals".into());
            }
            pos += 1;
            while pos < b.len() && b[pos].is_ascii_whitespace() {
                pos += 1;
            }
            let quote = *b.get(pos).ok_or("policy_xml_quote")?;
            if quote != b'\'' && quote != b'"' {
                return Err("policy_xml_quote".into());
            }
            pos += 1;
            let start = pos;
            while pos < b.len() && b[pos] != quote {
                if b[pos] == b'<' {
                    return Err("policy_xml_value".into());
                }
                pos += 1;
            }
            if pos >= b.len() {
                return Err("policy_xml_unclosed_value".into());
            }
            if attrs
                .insert(key, (xml[start..pos].into(), start..pos))
                .is_some()
            {
                return Err("policy_xml_duplicate_attribute".into());
            }
            pos += 1;
        }
        let insert = pos;
        let empty = b.get(pos) == Some(&b'/');
        if empty {
            pos += 1;
        }
        if b.get(pos) != Some(&b'>') {
            return Err("policy_xml_end".into());
        }
        pos += 1;
        if close {
            if empty || stack.pop().as_deref() != Some(&name) {
                return Err("policy_xml_unbalanced".into());
            }
        } else {
            if stack.is_empty() {
                roots += 1;
                if roots != 1 || name != "sys_thermal_control_list" {
                    return Err("policy_xml_root".into());
                }
            }
            stack.push(name);
            out.push(Tag {
                path: stack.clone(),
                attrs,
                insert,
            });
            if empty {
                stack.pop();
            }
        }
    }
    if !stack.is_empty() || roots != 1 {
        return Err("policy_xml_unbalanced".into());
    }
    Ok(out)
}

/// Only explicit engineer gears are copied. Safety/screen-off/complex/activity
/// policy trees are never changed. No extracted firmware is shipped in source.
pub fn transform(xml: &str) -> Result<String> {
    let parsed = tags(xml)?;
    for required in [
        vec!["sys_thermal_control_list", "thermalPolicyConfigItem"],
        vec![
            "sys_thermal_control_list",
            "thermalPolicyConfigItem",
            "specific",
            "com.oplus.engineermode",
        ],
        vec![
            "sys_thermal_control_list",
            "thermalPolicyConfigItem",
            "globalPolicy",
            "globalPolicy",
        ],
    ] {
        if parsed.iter().filter(|t| t.path == required).count() != 1 {
            return Err("policy_schema_ambiguous".into());
        }
    }
    let mut unique = std::collections::BTreeSet::new();
    for tag in &parsed {
        if tag.path.last().map(String::as_str) == Some("gear_config") {
            let gear = tag
                .attrs
                .get("tempGear")
                .ok_or("policy_missing_tempGear")?
                .0
                .parse::<u32>()
                .map_err(|_| "policy_bad_tempGear")?;
            if gear > 19 || !unique.insert((tag.path.clone(), gear)) {
                return Err("policy_gears_ambiguous".into());
            }
        }
    }
    let mut gears = BTreeMap::new();
    let number = |tag: &Tag, key: &str| -> Result<u32> {
        tag.attrs
            .get(key)
            .ok_or_else(|| format!("policy_missing_{key}"))?
            .0
            .parse::<u32>()
            .map_err(|_| format!("policy_bad_{key}"))
    };
    for tag in &parsed {
        if tag.path
            == [
                "sys_thermal_control_list",
                "thermalPolicyConfigItem",
                "specific",
                "com.oplus.engineermode",
                "gear_config",
            ]
        {
            let gear = number(tag, "tempGear")?;
            let charge = number(tag, "charge")?;
            if gear > 19 || charge > 30 || gears.insert(gear, charge).is_some() {
                return Err("policy_engineer_gears_invalid".into());
            }
        }
    }
    if gears.is_empty() {
        return Err("policy_engineer_gears_missing".into());
    }
    let mut edits = Vec::new();
    for tag in parsed {
        if tag.path.len() != 5
            || tag.path[1] != "thermalPolicyConfigItem"
            || tag.path[4] != "gear_config"
            || !matches!(
                tag.path[2].as_str(),
                "globalPolicy" | "specific" | "category" | "scene" | "default"
            )
        {
            continue;
        }
        let gear = number(&tag, "tempGear")?;
        if let Some(level) = gears.get(&gear) {
            if let Some((old, range)) = tag.attrs.get("charge") {
                let old = old.parse::<i32>().map_err(|_| "policy_bad_charge")?;
                // A policy sentinel/no-op must not be turned into a new request.
                if old < 0 {
                    continue;
                }
                edits.push((range.clone(), level.to_string()));
            } else {
                edits.push((tag.insert..tag.insert, format!(" charge=\"{level}\" ")));
            }
        }
    }
    let mut output = xml.to_owned();
    for (range, value) in edits.into_iter().rev() {
        output.replace_range(range, &value);
    }
    tags(&output)?;
    if output == xml {
        return Err("policy_no_changes".into());
    }
    Ok(output)
}

/// These are queries only. A provider denial/unknown format is not "no override".
pub fn sources_clear(h: &Hardware) -> Result<()> {
    if h.fixture {
        return if h.read("/engineer/online")? == "clear" {
            Ok(())
        } else {
            Err("policy_online_override".into())
        };
    }
    let active = command::text("/system/bin/am", &["get-current-user"], 2000, 1024)?;
    if active != "0" {
        return Err("policy_owner_user_required".into());
    }
    for name in [
        "sys_thermal_control_list",
        "sys_thermal_control_list_folding",
        "sys_thermal_control_list_split",
        "sys_thermal_control_list_low_health",
    ] {
        if command::text(
            "/system/bin/settings",
            &["get", "global", name],
            2000,
            16384,
        )? != "null"
        {
            return Err("policy_cosa_override_or_unknown".into());
        }
    }
    let result = command::text(
        "/system/bin/content",
        &[
            "query",
            "--user",
            "0",
            "--uri",
            "content://com.oplus.romupdate.provider.db/update_list",
            "--projection",
            "filtername",
            "--where",
            "filtername LIKE 'sys_thermal_control_list%'",
        ],
        3000,
        16384,
    )
    .map_err(|error| format!("policy_rus_query_failed:{error}"))?;
    rus_source_clear(&result)
}
fn rus_source_clear(result: &str) -> Result<()> {
    match result.trim() {
        "No result found." => Ok(()),
        "" => Err("policy_rus_query_empty".into()),
        value if value.starts_with("Row: ") => Err("policy_rus_override_present".into()),
        _ => Err("policy_rus_query_unrecognized".into()),
    }
}
pub fn identity(h: &Hardware) -> Result<()> {
    if h.fixture {
        return if h.read("/engineer/identity")? == "verified" {
            Ok(())
        } else {
            Err("policy_firmware_not_supported".into())
        };
    }
    let classpath = format!("-Djava.class.path={}/bin/cg-camera.jar", crate::MODULE);
    let output = command::text(
        "/system/bin/app_process",
        &[&classpath, "/system/bin", "com.chargeguard.EngineerInfo"],
        8000,
        4096,
    )
    .map_err(|e| format!("policy_battery_contract_check:{e}"))?;
    let info: serde_json::Value =
        serde_json::from_str(&output).map_err(|_| "policy_battery_contract_response")?;
    if info["api"] != 1
        || info["compatible"] != true
        || info["uid"] != 1000
        || info["process"] != "com.oplus.athena"
    {
        return Err(info["error"]
            .as_str()
            .filter(|s| s.starts_with("policy_") && s.len() < 256)
            .unwrap_or("policy_battery_contract_unsupported")
            .to_owned());
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    SourceReady,
    Mounting,
    ReloadApply,
    Applied,
    Unmounting,
    ReloadRestore,
    Restored,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub phase: Phase,
    pub original_hash: String,
    pub overlay_hash: String,
    pub source_stamp: Option<(u64, u64)>,
    pub app_pid: Option<u32>,
    pub app_start: Option<String>,
    #[serde(default)]
    pub reload_before: Option<(u32, String)>,
    #[serde(default)]
    pub reload_method: u32,
    pub source_checked_ms: u64,
    pub failed: bool,
}
impl Session {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        let s: Self =
            serde_json::from_str(raw.ok_or("policy_record_missing")?).map_err(|e| e.to_string())?;
        if [s.original_hash.as_str(), s.overlay_hash.as_str()]
            .iter()
            .any(|s| s.len() != 64 || !s.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            return Err("policy_record_invalid".into());
        }
        Ok(s)
    }
}
fn xml(h: &Hardware) -> Result<String> {
    String::from_utf8(bytes(h, TARGET, MAX_XML)?).map_err(|_| "policy_xml_encoding".into())
}
pub fn prepare(h: &Hardware) -> Result<Session> {
    identity(h)?;
    sources_clear(h)?;
    if h.has_mount_at(TARGET)? || h.present(SOURCE)? {
        return Err("policy_mount_or_source_conflict".into());
    }
    let input = xml(h)?;
    let original_hash = digest(input.as_bytes());
    let overlay = transform(&input)?;
    Ok(Session {
        phase: Phase::Prepared,
        original_hash,
        overlay_hash: digest(overlay.as_bytes()),
        source_stamp: None,
        app_pid: None,
        app_start: None,
        reload_before: None,
        reload_method: 2,
        source_checked_ms: crate::runtime::now_ms(),
        failed: false,
    })
}
fn stamp(h: &Hardware, path: &str) -> Result<(u64, u64)> {
    let m = fs::symlink_metadata(h.path(path)).map_err(|e| e.to_string())?;
    if !m.is_file() || m.file_type().is_symlink() {
        return Err("policy_not_regular".into());
    }
    Ok((m.dev(), m.ino()))
}
fn owned_source(h: &Hardware, s: &Session) -> Result<()> {
    if Some(stamp(h, SOURCE)?) != s.source_stamp || hash(h, SOURCE, MAX_XML)? != s.overlay_hash {
        return Err("policy_source_identity_changed".into());
    }
    Ok(())
}
fn create_source(
    h: &Hardware,
    s: &mut Session,
    persist: &mut impl FnMut(&Session) -> Result<()>,
) -> Result<()> {
    if hash(h, TARGET, MAX_XML)? != s.original_hash {
        return Err("policy_original_changed".into());
    }
    let output = transform(&xml(h)?)?;
    if digest(output.as_bytes()) != s.overlay_hash {
        return Err("policy_transform_changed".into());
    }
    h.prepare_masks()?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(h.path(SOURCE))
        .map_err(|e| e.to_string())?;
    s.source_stamp = Some(stamp(h, SOURCE)?);
    persist(s)?;
    file.write_all(output.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    let meta = fs::metadata(h.path(TARGET)).map_err(|e| e.to_string())?;
    if !h.fixture {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fchown(file.as_raw_fd(), meta.uid(), meta.gid()) } != 0 {
            return Err("policy_source_owner".into());
        }
        let label = command::text("/system/bin/ls", &["-Zd", TARGET], 1500, 2048)?;
        let context = label
            .split_whitespace()
            .find(|s| s.starts_with("u:object_r:"))
            .ok_or("policy_selinux_label_unknown")?;
        command::text("/system/bin/chcon", &[context, SOURCE], 1500, 1024)?;
        let actual = command::text("/system/bin/ls", &["-Zd", SOURCE], 1500, 2048)?;
        if !actual.split_whitespace().any(|x| x == context) {
            return Err("policy_selinux_label_mismatch".into());
        }
    }
    fs::set_permissions(
        h.path(SOURCE),
        fs::Permissions::from_mode(meta.mode() & 0o777),
    )
    .map_err(|e| e.to_string())?;
    s.source_stamp = Some(stamp(h, SOURCE)?);
    Ok(())
}
fn process(h: &Hardware) -> Result<(u32, String)> {
    if h.fixture {
        let pid = h
            .read("/engineer/pid")?
            .parse()
            .map_err(|_| "policy_app_pid")?;
        return Ok((pid, h.read("/engineer/start")?));
    }
    let mut found = Vec::new();
    for name in h.entries("/proc") {
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if bytes(h, &format!("/proc/{pid}/cmdline"), 4096)
            .ok()
            .is_none_or(|s| s.split(|b| *b == 0).next() != Some(b"com.oplus.athena".as_slice()))
        {
            continue;
        }
        if fs::metadata(h.path(&format!("/proc/{pid}")))
            .map_err(|e| e.to_string())?
            .uid()
            != 1000
        {
            continue;
        }
        let stat = h.read(&format!("/proc/{pid}/stat"))?;
        let start = stat
            .rsplit_once(") ")
            .and_then(|(_, s)| s.split_whitespace().nth(19))
            .ok_or("policy_app_stat")?;
        found.push((pid, start.to_owned()));
    }
    if found.len() != 1 {
        return Err("policy_app_process_unknown".into());
    }
    Ok(found.remove(0))
}
fn app_view(h: &Hardware, expected: &str) -> Result<(u32, String)> {
    let p = process(h)?;
    let value = if h.fixture {
        h.read("/engineer/view")?
    } else {
        hash(h, &format!("/proc/{}/root{TARGET}", p.0), MAX_XML)?
    };
    if value != expected {
        return Err("policy_app_mount_not_visible".into());
    }
    if process(h)? != p {
        return Err("policy_app_changed_during_check".into());
    }
    Ok(p)
}
fn terminate_process(h: &Hardware, expected: &(u32, String)) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // pidfd prevents a recycled PID from receiving our signal. The caller has
    // already persisted a reload checkpoint; no UID-wide kill or force-stop.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, expected.0, 0) };
    if fd < 0 {
        return Err(format!(
            "policy_pidfd_open:{}",
            std::io::Error::last_os_error()
        ));
    }
    let file = unsafe { fs::File::from_raw_fd(fd as i32) };
    if process(h).as_ref() != Ok(expected) {
        return Err("policy_app_changed_before_reload".into());
    }
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                file.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if rc < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(format!(
                "policy_app_signal:{}",
                std::io::Error::last_os_error()
            ));
        }
        let mut event = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut event, 1, 1500) };
        if rc > 0 && event.revents & libc::POLLIN != 0 {
            return Ok(());
        }
        if rc < 0 {
            return Err("policy_app_exit_wait_failed".into());
        }
    }
    Err("policy_app_exit_not_confirmed".into())
}
fn reload(h: &Hardware, expected: &str) -> Result<(u32, String)> {
    let before = process(h).ok();
    if h.fixture {
        h.record(serde_json::json!({"kind":"policy_reload","hash":expected}));
        if h.exists("/engineer/reload.fail") {
            return Err("policy_fixture_reload_failure".into());
        }
        let next = before.map_or(100, |p| p.0 + 1);
        fs::write(h.path("/engineer/pid"), next.to_string()).map_err(|e| e.to_string())?;
        fs::write(h.path("/engineer/start"), next.to_string()).map_err(|e| e.to_string())?;
        fs::write(h.path("/engineer/view"), expected).map_err(|e| e.to_string())?;
        return app_view(h, expected);
    }
    if let Some(ref previous) = before {
        terminate_process(h, previous)?;
    }
    let start = command::text(
        "/system/bin/am",
        &["startservice", "--user", "0", "-n", COMPONENT],
        5000,
        4096,
    )?;
    if !start.starts_with("Starting service:")
        || start.contains("Error")
        || start.contains("Exception")
    {
        return Err("policy_app_start_unconfirmed".into());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(p) = app_view(h, expected) {
            if Some(&p) != before.as_ref() {
                return Ok(p);
            }
        }
        if Instant::now() >= deadline {
            return Err("policy_app_reload_not_confirmed".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn apply(
    h: &Hardware,
    s: &mut Session,
    persist: &mut impl FnMut(&Session) -> Result<()>,
) -> Result<()> {
    if s.failed {
        return Err("policy_attempt_latched_change_config_to_retry".into());
    }
    if h.service_state("horae")?.as_deref() != Some("running") {
        return Err("policy_horae_not_running".into());
    }
    if s.phase == Phase::Applied {
        if s.source_checked_ms == 0
            || crate::runtime::now_ms() < s.source_checked_ms
            || crate::runtime::now_ms().saturating_sub(s.source_checked_ms) >= 60_000
        {
            identity(h)?;
            sources_clear(h)?;
            s.source_checked_ms = crate::runtime::now_ms();
            persist(s)?;
        }
        owned_source(h, s)?;
        if !h.mounted(TARGET, SOURCE) || hash(h, TARGET, MAX_XML)? != s.overlay_hash {
            return Err("policy_mount_lost".into());
        }
        let p = app_view(h, &s.overlay_hash)?;
        s.app_pid = Some(p.0);
        s.app_start = Some(p.1);
        return Ok(());
    }
    if s.phase == Phase::Prepared {
        create_source(h, s, persist)?;
        s.phase = Phase::SourceReady;
        persist(s)?;
    }
    if s.phase == Phase::SourceReady {
        owned_source(h, s)?;
        sources_clear(h)?;
        s.phase = Phase::Mounting;
        persist(s)?;
        h.bind(SOURCE, TARGET)?;
        if hash(h, TARGET, MAX_XML)? != s.overlay_hash {
            return Err("policy_mount_hash_mismatch".into());
        }
        s.phase = Phase::ReloadApply;
        s.reload_method = 2;
        s.reload_before = process(h).ok();
        persist(s)?;
        let p = reload(h, &s.overlay_hash)?;
        s.app_pid = Some(p.0);
        s.app_start = Some(p.1);
        s.phase = Phase::Applied;
        persist(s)?;
        return Ok(());
    }
    // An interrupted mutating phase is recovered, never blindly replayed.
    Err("policy_interrupted_apply_requires_restore".into())
}
pub fn restore(
    h: &Hardware,
    s: &mut Session,
    persist: &mut impl FnMut(&Session) -> Result<()>,
) -> Result<()> {
    if s.phase == Phase::Restored {
        return Ok(());
    }
    let mounted = h.has_mount_at(TARGET)?;
    if mounted {
        owned_source(h, s)?;
        if !h.mounted(TARGET, SOURCE) {
            return Err("policy_foreign_mount".into());
        }
    }
    let touched = matches!(
        s.phase,
        Phase::Mounting
            | Phase::ReloadApply
            | Phase::Applied
            | Phase::Unmounting
            | Phase::ReloadRestore
    );
    if s.phase != Phase::ReloadRestore {
        if touched {
            s.phase = Phase::Unmounting;
            persist(s)?;
        }
        if mounted {
            h.unbind(TARGET, SOURCE)?;
        }
        if hash(h, TARGET, MAX_XML)? != s.original_hash {
            return Err("policy_restore_original_mismatch".into());
        }
        if touched {
            s.phase = Phase::ReloadRestore;
            s.reload_method = 2;
            s.reload_before = process(h).ok();
            persist(s)?;
            let p = reload(h, &s.original_hash)?;
            s.app_pid = Some(p.0);
            s.app_start = Some(p.1);
        }
    } else {
        if mounted {
            return Err("policy_restore_mount_reappeared".into());
        }
        // A previous reload was issued; read-only confirmation only, no restart loop.
        if hash(h, TARGET, MAX_XML)? != s.original_hash {
            return Err("policy_restore_original_mismatch".into());
        }
        if s.reload_method < 2 {
            // Upgrade a legacy force-stop checkpoint once, including unplugged
            // recovery. Persist consumption before touching the exact process.
            s.reload_method = 2;
            s.reload_before = process(h).ok();
            persist(s)?;
            let p = reload(h, &s.original_hash)?;
            s.app_pid = Some(p.0);
            s.app_start = Some(p.1);
        } else {
            let current = app_view(h, &s.original_hash)?;
            if Some(current) == s.reload_before {
                return Err("policy_restore_reload_not_observed".into());
            }
        }
    }
    if h.present(SOURCE)? {
        if Some(stamp(h, SOURCE)?) != s.source_stamp {
            return Err("policy_source_restore_identity_changed".into());
        }
        fs::remove_file(h.path(SOURCE)).map_err(|e| e.to_string())?;
    }
    s.phase = Phase::Restored;
    persist(s)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rus_query_requires_an_explicit_empty_result() {
        assert!(rus_source_clear("No result found.\n").is_ok());
        for (output, error) in [
            ("", "policy_rus_query_empty"),
            ("  \n", "policy_rus_query_empty"),
            (
                "Row: 0 filtername=sys_thermal_control_list",
                "policy_rus_override_present",
            ),
            (
                "Error while accessing provider",
                "policy_rus_query_unrecognized",
            ),
            ("Warning\nNo result found.", "policy_rus_query_unrecognized"),
        ] {
            assert_eq!(rus_source_clear(output).unwrap_err(), error);
        }
    }
    #[test]
    fn charge_only_overlay_leaves_safety_and_other_gears_intact() {
        let xml = r#"<sys_thermal_control_list><thermalPolicyConfigItem><globalPolicy><globalPolicy><gear_config tempGear="3" cpu="2" charge="7"/><gear_config tempGear="8" charge="3"/></globalPolicy></globalPolicy><specific><com.oplus.engineermode><gear_config tempGear="3" charge="30"/></com.oplus.engineermode></specific><safetyTest><safety_test><gear_config tempGear="3" charge="1"/></safety_test></safetyTest></thermalPolicyConfigItem></sys_thermal_control_list>"#;
        let output = transform(xml).unwrap();
        assert_eq!(
            output,
            xml.replacen("cpu=\"2\" charge=\"7\"", "cpu=\"2\" charge=\"30\"", 1)
        );
        assert!(transform(&xml.replace("</specific>", "</unknown>")).is_err());
        assert!(transform(&xml.replace("charge=\"30\"", "charge=\"31\"")).is_err());
    }
}
