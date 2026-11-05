//! A narrowly scoped optional LKM; never writes OEM votes, PDOs or debug sensors.
use crate::{hardware::Hardware, storage, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const ID: &str = "pps_status_assist";
pub const MODULE_NAME: &str = "charge_guard_pps";
pub const ENABLED: &str = "/sys/module/charge_guard_pps/parameters/enabled";
pub const STATUS: &str = "/sys/module/charge_guard_pps/parameters/status";
pub const DIAGNOSTICS: &str = "/sys/module/charge_guard_pps/parameters/diagnostics";
pub const EVENTS: &str = "/sys/module/charge_guard_pps/parameters/events";
pub const RECOVERY: &str = "/sys/module/charge_guard_pps/parameters/recovery";
pub const DRIVER_ID: &str = "pjz110-f04-205fb7eb-v1";
pub const DRIVER_IMAGE: &str = "205fb7ebdd5d12438fbd40e7432d12c034929c629a8657ca17f38d50de7c63ce";
pub const KERNEL_RELEASE: &str = "6.6.118-android15-8-g9bc34d5b0c79-abogki537459655-4k";

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub ready: bool,
    pub enabled: bool,
    pub applied: bool,
    pub retiring: bool,
    pub observations: u64,
    pub diagnostics: Option<Diagnostics>,
    pub diagnostic_error: Option<String>,
    pub recovery: Option<Recovery>,
    pub recovery_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    pub api: u32,
    pub ready: bool,
    pub state: String,
    pub error: i32,
    pub checked_ms: u64,
    pub temperature_decic: i32,
    pub real_type: i32,
    pub protocol: i32,
    pub attempts: u32,
    pub successes: u32,
    pub last_request_ms: u64,
    pub blocking_voter: String,
}

pub fn parse_recovery(raw: &str) -> Result<Recovery> {
    let r: Recovery =
        serde_json::from_str(raw).map_err(|e| format!("pps_recovery_json:{e}"))?;
    if r.api != 1
        || !matches!(r.state.as_str(), "off" | "observing" | "active" | "waiting_cool"
            | "waiting_conditions" | "settling" | "requesting" | "recovered"
            | "retry_exhausted" | "unsupported" | "retired" | "request_failed")
        || !matches!(r.protocol, -1 | 0 | 3 | 4)
        || r.real_type < -1
        || !valid_voter(&r.blocking_voter)
    {
        return Err("pps_recovery_fields".into());
    }
    Ok(r)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostics {
    pub api: u32,
    pub ready: bool,
    pub error: i32,
    pub generation_ms: u64,
    pub seq: u64,
    pub overwritten: u64,
    pub state: String,
    pub charging: bool,
    pub support: i32,
    pub cp_ibus: i32,
    pub queries: u64,
    pub successes: u64,
    pub failures: u64,
    pub uses: u64,
    pub resets: u64,
    pub query_ms: u64,
    pub rc: i32,
    pub raw: Option<u32>,
    pub used_ms: u64,
    pub exit: String,
    pub exit_voter: String,
    pub disable_vote: String,
    pub not_allow_vote: String,
}

fn valid_voter(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn parse_diagnostics(raw: &str) -> Result<Diagnostics> {
    let d: Diagnostics =
        serde_json::from_str(raw).map_err(|e| format!("pps_diagnostics_json:{e}"))?;
    if d.api != 1
        || !matches!(
            d.state.as_str(),
            "off"
                | "waiting_pps"
                | "armed"
                | "used"
                | "data_unavailable"
                | "exited"
                | "retired"
                | "unavailable"
        )
        || !matches!(d.exit.as_str(), "none" | "soft" | "force")
        || !(-1..=1).contains(&d.support)
        || !(-1..=1).contains(&d.cp_ibus)
        || [&d.exit_voter, &d.disable_vote, &d.not_allow_vote]
            .iter()
            .any(|s| !valid_voter(s))
    {
        return Err("pps_diagnostics_fields".into());
    }
    Ok(d)
}

/// seq, kernel boottime, kind, query phase, result/value, raw reply (-1 absent),
/// OEM support flag, observed current, effective voter. No inferred power reading.
#[derive(Debug, Serialize, Deserialize)]
struct NativeEvent(u64, u64, u8, u8, i32, i64, i32, i32, String);

#[derive(Debug, Deserialize)]
struct EventBatch {
    api: u32,
    generation_ms: u64,
    first_seq: u64,
    next_seq: u64,
    overwritten: u64,
    events: Vec<NativeEvent>,
}

fn parse_events(raw: &str) -> Result<EventBatch> {
    let batch: EventBatch =
        serde_json::from_str(raw).map_err(|e| format!("pps_events_json:{e}"))?;
    if batch.api != 1
        || batch.events.len() > 16
        || batch.first_seq == 0
        || batch.next_seq.checked_sub(batch.first_seq) != Some(batch.events.len() as u64)
        || batch.events.iter().enumerate().any(|(i, e)| {
            e.0 != batch.first_seq + i as u64
                || !(1..=12).contains(&e.2)
                || e.3 > 2
                || !(-1..=u32::MAX as i64).contains(&e.5)
                || !(-1..=1).contains(&e.6)
                || !valid_voter(&e.8)
        })
    {
        return Err("pps_events_fields".into());
    }
    Ok(batch)
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    boot_id: String,
    generation_ms: u64,
    next_seq: u64,
}

fn event_name(kind: u8) -> &'static str {
    match kind {
        1 => "bound",
        2 => "query",
        3 => "support",
        4 => "used",
        5 => "charging",
        6 => "disable_vote",
        7 => "not_allow_vote",
        8 => "soft_exit",
        9 => "force_exit",
        10 => "hard_reset",
        11 => "enabled",
        12 => "retiring",
        _ => unreachable!(),
    }
}

fn late_context(h: &Hardware) -> Value {
    let read = |p| match h.read(p) {
        Ok(v) => json!({"value":v}),
        Err(e) => json!({"error":e}),
    };
    json!({"captured_boottime_ms":crate::runtime::now_ms(),"timing":"userspace_after_event",
        "battery":crate::runtime::battery(h),
        "pps_disable":read("/proc/oplus-votable/PPS_DISABLE/status"),
        "pps_not_allow":read("/proc/oplus-votable/PPS_NOT_ALLOW/status"),
        "pps_current":read("/proc/oplus-votable/PPS_CURR/status")})
}

fn append_diagnostic(h: &Hardware, event: &str, detail: Value, detailed: bool) -> Result<()> {
    storage::log(
        &h.state(),
        &crate::runtime::event_record(event, detail.clone()),
    )?;
    if detailed {
        crate::runtime::detail_log(h, event, detail)?;
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct RecoverySnapshot {
    boot_id: String,
    generation_ms: Option<u64>,
    state: String,
    attempts: u32,
    blocking_voter: String,
    error: i32,
}

fn capture_recovery(h: &Hardware) -> Result<()> {
    if !h.present(RECOVERY)? {
        return Ok(());
    }
    let helper = status(h)?;
    let r = helper.recovery.ok_or_else(|| {
        helper
            .recovery_error
            .unwrap_or_else(|| "pps_recovery_unavailable".into())
    })?;
    let snapshot = RecoverySnapshot {
        boot_id: h.read("/proc/sys/kernel/random/boot_id")?,
        generation_ms: helper.diagnostics.map(|d| d.generation_ms),
        state: r.state.clone(),
        attempts: r.attempts,
        blocking_voter: r.blocking_voter.clone(),
        error: r.error,
    };
    let path = h.state().join("pps-recovery-snapshot.json");
    if path.exists() {
        let previous: RecoverySnapshot = storage::load(&path, 4096)?;
        if previous.boot_id == snapshot.boot_id
            && previous.generation_ms == snapshot.generation_ms
            && previous.state == snapshot.state
            && previous.attempts == snapshot.attempts
            && previous.blocking_voter == snapshot.blocking_voter
            && previous.error == snapshot.error
        {
            return Ok(());
        }
    }
    append_diagnostic(
        h,
        "pps_recovery_state",
        json!(r),
        crate::runtime::config(h).is_ok_and(|c| c.detailed_logging),
    )?;
    storage::json(&path, &snapshot)
}

/// Called only by the existing PPS maintenance/restore paths. A cursor is
/// acknowledged after the corresponding log writes; readers never consume the
/// kernel ring. Stable event IDs identify replay after a cursor-save interruption.
pub fn capture_events(h: &Hardware) -> Result<()> {
    let _lock = storage::Lock::take(&h.state().join("pps-events.lock"))?;
    let events = capture_native_events(h);
    let recovery = capture_recovery(h);
    events.and(recovery)
}

fn capture_native_events(h: &Hardware) -> Result<()> {
    if !h.present(EVENTS)? {
        return Ok(());
    } // Old helpers remain restorable.
    let batch = parse_events(&h.read(EVENTS)?)?;
    let path = h.state().join("pps-events-cursor.json");
    let boot_id = h.read("/proc/sys/kernel/random/boot_id")?;
    let mut cursor = if path.exists() {
        storage::load::<Cursor>(&path, 4096)?
    } else {
        Cursor {
            boot_id: boot_id.clone(),
            generation_ms: batch.generation_ms,
            next_seq: 1,
        }
    };
    if cursor.boot_id != boot_id || cursor.generation_ms != batch.generation_ms {
        cursor = Cursor {
            boot_id: boot_id.clone(),
            generation_ms: batch.generation_ms,
            next_seq: 1,
        };
    }
    if cursor.next_seq == 0 || cursor.next_seq > batch.next_seq {
        return Err("pps_cursor_sequence_invalid".into());
    }
    if cursor.next_seq == batch.next_seq {
        return Ok(());
    }
    // Critical events do not depend on config validity during stop/recovery.
    // Detailed records require an explicitly enabled, readable saved setting.
    let detailed = crate::runtime::config(h).is_ok_and(|c| c.detailed_logging);
    if cursor.next_seq < batch.first_seq {
        append_diagnostic(
            h,
            "pps_diagnostic_gap",
            json!({
            "event_id":format!("{}/{}/gap-{}-{}",boot_id,batch.generation_ms,cursor.next_seq,batch.first_seq),
            "generation_ms":batch.generation_ms,"first_missing_seq":cursor.next_seq,
            "next_available_seq":batch.first_seq,"lost":batch.first_seq-cursor.next_seq,
            "buffer_overwrites":batch.overwritten}),
            detailed,
        )?;
        cursor.next_seq = batch.first_seq;
        storage::json(&path, &cursor)?;
    }
    let mut context = None;
    for e in batch.events {
        if e.0 < cursor.next_seq {
            continue;
        }
        let exceptional =
            matches!(e.2, 8 | 9 | 10 | 12) || e.2 == 2 && e.4 < 0 || e.2 == 3 && e.4 == 0;
        if exceptional && context.is_none() {
            context = Some(late_context(h));
        }
        let mut detail = json!({"event_id":format!("{}/{}/{}",boot_id,batch.generation_ms,e.0),
            "generation_ms":batch.generation_ms,"seq":e.0,"occurred_boottime_ms":e.1,
            "kind":event_name(e.2),"query_phase":match e.3 {1=>"startup",2=>"steady",_=>"none"},
            "result":e.4,"raw_reply":if e.5<0 {None}else{Some(e.5 as u32)},
            "oem_support":e.6,"current_ma":e.7,"effective_voter":e.8,
            "query_semantics":"return_success_is_not_freshness_or_use"});
        if exceptional {
            detail["context"] = json!(context);
        }
        append_diagnostic(h, "pps_diagnostic", detail, detailed)?;
        cursor.next_seq = e.0 + 1;
        storage::json(&path, &cursor)?;
    }
    Ok(())
}

pub fn parse_status(raw: &str) -> Result<Status> {
    let mut fields = BTreeMap::new();
    for field in raw.split_whitespace() {
        let (key, value) = field.split_once('=').ok_or("pps_status_invalid")?;
        if fields.insert(key, value).is_some() {
            return Err("pps_status_duplicate_field".into());
        }
    }
    if fields.get("api") != Some(&"1") || fields.get("driver") != Some(&DRIVER_ID) {
        return Err("pps_helper_identity_mismatch".into());
    }
    let boolean = |key| match fields.get(key) {
        Some(&"0") => Ok(false),
        Some(&"1") => Ok(true),
        _ => Err(format!("pps_status_invalid:{key}")),
    };
    Ok(Status {
        ready: boolean("ready")?,
        enabled: boolean("enabled")?,
        applied: boolean("applied")?,
        retiring: boolean("retiring")?,
        observations: fields
            .get("observations")
            .ok_or("pps_status_missing_observations")?
            .parse()
            .map_err(|_| "pps_status_invalid_observations")?,
        diagnostics: None,
        diagnostic_error: None,
        recovery: None,
        recovery_error: None,
    })
}

pub fn status(h: &Hardware) -> Result<Status> {
    let mut status = parse_status(&h.read(STATUS)?)?;
    let diagnostic = (|| -> Result<Option<Diagnostics>> {
        if !h.present(DIAGNOSTICS)? {
            return Ok(None);
        }
        parse_diagnostics(&h.read(DIAGNOSTICS)?).map(Some)
    })();
    match diagnostic {
        Ok(d) => status.diagnostics = d,
        Err(e) => status.diagnostic_error = Some(e),
    }
    let recovery = (|| -> Result<Option<Recovery>> {
        if !h.present(RECOVERY)? {
            return Ok(None);
        }
        parse_recovery(&h.read(RECOVERY)?).map(Some)
    })();
    match recovery {
        Ok(r) => status.recovery = r,
        Err(e) => status.recovery_error = Some(e),
    }
    Ok(status)
}

/// One presentation result for the preset and backend details. "applied" in the
/// legacy helper status remains an ownership flag, never proof of current use.
pub fn presentation(s: &Status) -> (&'static str, String) {
    if s.retiring {
        return ("pps_retired", "驱动已退出，辅助功能已停止".into());
    }
    if !s.enabled {
        return ("pps_off", "辅助开关未启用".into());
    }
    let data_note = if s.diagnostics.as_ref().is_some_and(|d| {
        d.ready && (d.support == 0 || d.state == "data_unavailable")
    }) {
        "；PPS Status 数据通道不可用"
    } else {
        ""
    };
    if let Some(r) = &s.recovery {
        let (state, message) = if r.state == "retired" {
            ("pps_retired", "驱动已退出，辅助功能已停止")
        } else if !r.ready {
            ("pps_recovery_unavailable", "自动恢复接口未就绪")
        } else {
            match r.state.as_str() {
                "off" => ("pps_recovery_off", "自动恢复维护已关闭"),
                "observing" => ("pps_recovery_observing", "正在观察原厂充电状态"),
                "active" => ("pps_recovery_active", "当前实际处于 PPS 充电"),
                "waiting_cool" => ("pps_recovery_waiting_cool", "等待真实电池温度降至恢复条件"),
                "waiting_conditions" => ("pps_recovery_waiting_conditions", "等待原厂 PPS 准入条件"),
                "settling" => ("pps_recovery_settling", "PPS 退出后等待协议稳定"),
                "requesting" => ("pps_recovery_requesting", "已请求原厂重新评估，等待实际进入 PPS"),
                "recovered" => ("pps_recovery_recovered", "已确认重新进入 PPS 充电"),
                "retry_exhausted" => ("pps_recovery_retry_exhausted", "本轮恢复尝试已用尽，保留原厂充电状态"),
                "retired" => ("pps_retired", "驱动已退出，辅助功能已停止"),
                "request_failed" => ("pps_recovery_request_failed", "原厂 PPS 重入请求失败，请查看后端详情"),
                _ => ("pps_recovery_unavailable", "当前固件未适配自动恢复接口"),
            }
        };
        return (state, format!("{message}{data_note}"));
    }
    if s.recovery_error.is_some() {
        return ("pps_recovery_unavailable", format!("自动恢复状态读取失败，请查看后端详情{data_note}"));
    }
    let (state, message) = diagnostic_presentation(s);
    (state, format!("{message}；当前辅助模块不具备自动恢复能力"))
}

fn diagnostic_presentation(s: &Status) -> (&'static str, String) {
    let Some(d) = s.diagnostics.as_ref().filter(|d| d.ready) else {
        return (
            "pps_diagnostic_unavailable",
            "辅助开关已设置；诊断信息不可用".into(),
        );
    };
    match d.state.as_str() {
        "waiting_pps" => ("pps_waiting", "已准备，等待 PPS 充电流程".into()),
        "used" => (
            "pps_used",
            "源端电流已实际参与判断；不代表功率或数据新鲜度已验证".into(),
        ),
        "data_unavailable" => (
            "pps_data_unavailable",
            "原厂已停用源端数据通道；请查看查询结果".into(),
        ),
        "exited" => (
            "pps_exited",
            format!("已退出 PPS；{}", exit_reason(&d.exit_voter)),
        ),
        "retired" => ("pps_retired", "驱动已退出，辅助功能已停止".into()),
        "unavailable" => (
            "pps_diagnostic_unavailable",
            "辅助开关已设置；诊断信息不可用".into(),
        ),
        _ => ("pps_armed", "辅助开关已设置，尚无实际参与判断的证据".into()),
    }
}

fn exit_reason(voter: &str) -> String {
    match voter {
        "none" | "unknown" => "原因未知，请查看事件顺序".into(),
        "PPS_IBAT_ABNOR_VOTER" => "原厂低电流异常票生效".into(),
        "CHG_FULL_VOTER" | "CHG_FULL_COOL_VOTER" => "原厂满电/收尾票生效".into(),
        "BATT_TEMP_VOTER" | "BTB_TEMP_OVER_VOTER" => "原厂温度禁止票生效".into(),
        "IBAT_OVER_VOTER" => "原厂电流保护票生效".into(),
        "TIMEOUT_VOTER" => "原厂充电超时票生效".into(),
        other => format!("生效禁止票 {other}"),
    }
}

pub fn original(h: &Hardware) -> Result<String> {
    if !h.present("/sys/module/charge_guard_pps")? {
        // This module is shipped disabled, and cannot accept enabled=1 at load time.
        return Ok("0".into());
    }
    status(h)?;
    let value = h.read(ENABLED)?;
    if !matches!(value.as_str(), "0" | "1") {
        return Err("pps_helper_value_invalid".into());
    }
    Ok(value)
}

/// Called only after the Controller has persisted its recovery record.
pub fn prepare(h: &Hardware) -> Result<()> {
    if !h.capabilities().pps_verified {
        return Err("pps_firmware_not_supported".into());
    }
    crate::capabilities::confirm_loaded(h)?;
    if !h.present("/sys/module/charge_guard_pps")? {
        load(h)?;
    }
    let s = status(h)?;
    if !s.ready || s.retiring {
        return Err("pps_driver_retired_or_not_ready".into());
    }
    if s.enabled && h.present(RECOVERY)? {
        h.write(RECOVERY, "1")?;
    }
    Ok(())
}

pub fn restore(h: &Hardware, original: Option<&str>) -> Result<Option<String>> {
    let value = original
        .filter(|v| matches!(*v, "0" | "1"))
        .ok_or("pps_original_invalid")?;
    if !h.present("/sys/module/charge_guard_pps")? {
        return if value == "0" {
            Ok(None)
        } else {
            Err("pps_restore_module_missing".into())
        };
    }
    status(h)?;
    let mut log_error = capture_events(h).err();
    if let Some(error) = &log_error {
        eprintln!("pps_diagnostic_log_failed_before_restore:{error}");
    }
    h.write(ENABLED, value)?;
    let s = status(h)?;
    if h.read(ENABLED)? != value || (value == "0" && (s.enabled || s.applied)) {
        return Err("pps_restore_not_confirmed".into());
    }
    // Disable freezes observation before the final read. Logging is evidence,
    // not a prerequisite for restoring the user's charging behavior.
    if let Err(error) = capture_events(h) {
        eprintln!("pps_diagnostic_log_failed_after_restore:{error}");
        log_error = Some(error);
    }
    // Keep a pre-existing enabled helper intact; only unload after restoring off.
    if value == "0" {
        unload(h)?;
    }
    Ok(log_error)
}

fn load(h: &Hardware) -> Result<()> {
    #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
    if h.fixture {
        let dir = h.path("/sys/module/charge_guard_pps/parameters");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("enabled"), "0").map_err(|e| e.to_string())?;
        std::fs::write(
            dir.join("status"),
            format!(
                "api=1 driver={DRIVER_ID} ready=1 enabled=0 applied=0 retiring=0 observations=0\n"
            ),
        )
        .map_err(|e| e.to_string())?;
        h.record(serde_json::json!({"kind":"pps_lkm_load"}));
        return Ok(());
    }
    #[cfg(target_os = "android")]
    {
        use std::os::fd::AsRawFd;
        let file =
            std::fs::File::open(h.path(&format!("{}/bin/charge_guard_pps.ko", crate::MODULE)))
                .map_err(|e| format!("pps_lkm_open:{e}"))?;
        let result =
            unsafe { libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), b"\0".as_ptr(), 0) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(format!("pps_lkm_load:{error}"));
            }
        }
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    Err("pps_lkm_requires_android".into())
}

fn unload(h: &Hardware) -> Result<()> {
    let _ = h;
    #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
    if h.fixture {
        std::fs::remove_dir_all(h.path("/sys/module/charge_guard_pps"))
            .map_err(|e| e.to_string())?;
        h.record(serde_json::json!({"kind":"pps_lkm_unload"}));
        return Ok(());
    }
    #[cfg(target_os = "android")]
    {
        let name = std::ffi::CString::new(MODULE_NAME).unwrap();
        // No force flags. EBUSY remains restore_pending and retains ownership.
        if unsafe { libc::syscall(libc::SYS_delete_module, name.as_ptr(), libc::O_NONBLOCK) } != 0 {
            return Err(format!(
                "pps_lkm_unload:{}",
                std::io::Error::last_os_error()
            ));
        }
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    Err("pps_lkm_requires_android".into())
}
