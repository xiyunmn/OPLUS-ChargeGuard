use crate::{
    camera::{Monitor, Usage},
    config::{self, Config, HoraeMode, WriteMode},
    control::{self, Controller},
    discovery,
    events::Events,
    hardware::{Hardware, GAUGE},
    maintenance::Schedule,
    maintenance_events::ControlEvents,
    storage, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::MetadataExt,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
static EXIT: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);
extern "C" fn signal(s: i32) {
    if s == libc::SIGUSR1 {
        RELOAD.store(true, Ordering::Relaxed);
    } else {
        EXIT.store(true, Ordering::Relaxed);
    }
    crate::events::notify();
}
fn signals() {
    unsafe {
        libc::signal(libc::SIGTERM, signal as usize);
        libc::signal(libc::SIGINT, signal as usize);
        libc::signal(libc::SIGUSR1, signal as usize);
    }
}
pub fn now_ms() -> u64 {
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut t) };
    t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000
}
fn worker_cpu_ms() -> Option<u64> {
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    (unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t) } == 0)
        .then_some(t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Identity {
    pid: u32,
    start_ticks: u64,
    boot_id: String,
    exe_dev: u64,
    exe_ino: u64,
}
fn ticks(pid: u32) -> Result<u64> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| e.to_string())?;
    let tail = s.rsplit_once(')').ok_or("invalid_proc_stat")?.1;
    if matches!(tail.split_whitespace().next(), Some("Z" | "X" | "x")) {
        return Err("process_exited".into());
    }
    tail.split_whitespace()
        .nth(19)
        .ok_or("missing_start_ticks")?
        .parse()
        .map_err(|_| "invalid_start_ticks".into())
}
fn alive(i: &Identity) -> bool {
    process_identity(i.pid).is_ok_and(|n| {
        n.start_ticks == i.start_ticks
            && n.boot_id == i.boot_id
            && n.exe_dev == i.exe_dev
            && n.exe_ino == i.exe_ino
    })
}
fn process_identity(pid: u32) -> Result<Identity> {
    let start_ticks = ticks(pid)?;
    let m = fs::metadata(format!("/proc/{pid}/exe")).map_err(|e| e.to_string())?;
    let boot_id =
        fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|e| e.to_string())?;
    if ticks(pid)? != start_ticks {
        return Err("process_changed".into());
    }
    Ok(Identity {
        pid,
        start_ticks,
        boot_id: boot_id.trim().into(),
        exe_dev: m.dev(),
        exe_ino: m.ino(),
    })
}
fn identity(h: &Hardware, name: &str) -> Option<Identity> {
    storage::load(&h.state().join(format!("{name}.json")), 4096).ok()
}
fn save_pid(h: &Hardware, name: &str) -> Result<Identity> {
    let i = process_identity(std::process::id())?;
    storage::json(&h.state().join(format!("{name}.json")), &i)?;
    Ok(i)
}
fn signal_pid(i: &Identity, sig: i32) -> Result<()> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, i.pid, 0) };
    if fd < 0 {
        return if alive(i) {
            Err(std::io::Error::last_os_error().to_string())
        } else {
            Ok(())
        };
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    if !alive(i) {
        return Ok(());
    }
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}
fn remove_pid(h: &Hardware, name: &str, i: &Identity) {
    if identity(h, name).is_some_and(|x| x.pid == i.pid && x.start_ticks == i.start_ticks) {
        let _ = fs::remove_file(h.state().join(format!("{name}.json")));
    }
}
fn inactive(h: &Hardware) -> bool {
    EXIT.load(Ordering::Relaxed)
        || h.state().join("manual-stop").exists()
        || h.exists(&format!("{}/disable", crate::MODULE))
        || h.exists(&format!("{}/remove", crate::MODULE))
}
pub fn init(h: &Hardware) -> Result<()> {
    storage::secure_dir(&h.state())?;
    if storage::load::<Config>(&h.state().join("config.json"), 16384)
        .is_ok_and(|c| c.validate().is_ok())
    {
        return Ok(());
    }
    let _lock = storage::Lock::take(&h.state().join("config.lock"))?;
    let p = h.state().join("config.json");
    if !p.exists() {
        storage::json(&p, &Config::default())?;
    } else if let Ok(mut c) = storage::load::<Config>(&p, 16384) {
        if c.migrate() {
            c.validate()?;
            storage::atomic(
                &h.state().join("config.pre-beta2.json"),
                storage::read(&p, 16384)?.as_bytes(),
            )?;
            storage::json(&p, &c)?;
        }
    }
    config(h).map(|_| ())
}
pub fn config(h: &Hardware) -> Result<Config> {
    let c: Config = storage::load(&h.state().join("config.json"), 16384)?;
    c.validate()?;
    Ok(c)
}
fn change_enabled(h: &Hardware, enabled: bool) -> Result<()> {
    let _lock = storage::Lock::take(&h.state().join("config.lock"))?;
    let mut c = config(h)?;
    c.enabled = enabled;
    c.revision += 1;
    storage::json(&h.state().join("config.json"), &c)
}
pub fn configure(h: &Hardware, payload: &str) -> Result<Value> {
    let mut ch: config::Change =
        serde_json::from_slice(&config::decode_hex(payload)?).map_err(|e| e.to_string())?;
    ch.config.validate()?;
    let _lock = storage::Lock::take(&h.state().join("config.lock"))?;
    let old = config(h)?;
    if ch.expected_revision != old.revision {
        return Err("revision_conflict".into());
    }
    // Lifecycle is changed only through start/stop, never incidentally by an old WebUI save.
    ch.config.enabled = old.enabled;
    ch.config.revision = old.revision + 1;
    storage::json(&h.state().join("config.json"), &ch.config)?;
    if let Some(i) = identity(h, "worker") {
        let _ = signal_pid(&i, libc::SIGUSR1);
    }
    Ok(json!({"saved_revision":ch.config.revision,"config":ch.config}))
}
fn event_record(event: &str, detail: Value) -> Value {
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .ok();
    json!({"event":event,"boottime_ms":now_ms(),"unix_ms":unix_ms,"detail":detail})
}
#[cfg(test)]
mod log_tests {
    use super::*;

    #[test]
    fn records_capture_wall_clock_without_replacing_monotonic_time() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let detail = json!({"profile":"charging"});
        let record = event_record("profile_changed", detail.clone());
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let stamp = record["unix_ms"].as_u64().unwrap();
        assert!((before..=after).contains(&stamp));
        assert!(record["boottime_ms"].as_u64().unwrap() <= now_ms());
        assert_eq!(record["event"], "profile_changed");
        assert_eq!(record["detail"], detail);
    }
}
fn detail_log(h: &Hardware, event: &str, detail: Value) -> Result<()> {
    let mut record = event_record(event, detail);
    record["worker_pid"] = json!(std::process::id());
    record["version"] = json!(crate::VERSION);
    storage::append_rotating(
        &h.state(),
        "detail.jsonl",
        &record,
        storage::DETAIL_LOG_FILE_BYTES,
        storage::DETAIL_LOG_BACKUPS,
    )
}
fn log(h: &Hardware, event: &str, detail: Value) {
    let _ = storage::log(&h.state(), &event_record(event, detail));
}
fn spawn_role(h: &Hardware, role: &str) -> Result<std::process::Child> {
    let p = h.state().join(format!("{role}.log"));
    storage::atomic(&p, b"")?;
    let stderr = fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .map_err(|e| e.to_string())?;
    Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .arg(role)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|e| e.to_string())
}
pub fn start(h: &Hardware) -> Result<Value> {
    let _lock = storage::Lock::take(&h.state().join("lifecycle.lock"))?;
    if h.exists(&format!("{}/disable", crate::MODULE))
        || h.exists(&format!("{}/remove", crate::MODULE))
    {
        return Err("module_disabled_or_removing".into());
    }
    change_enabled(h, true)?;
    let _ = fs::remove_file(h.state().join("manual-stop"));
    if identity(h, "guardian").is_some_and(|i| alive(&i)) {
        return Ok(json!({"started":true,"already_running":true}));
    }
    let mut child = spawn_role(h, "daemon")?;
    for _ in 0..40 {
        if identity(h, "guardian").is_some_and(|i| alive(&i)) {
            return Ok(json!({"started":true}));
        }
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("guardian_start_failed:{s}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(json!({"started":true,"status":"starting"}))
}
pub fn daemon(h: &Hardware) -> Result<Value> {
    signals();
    let mut events = Events::new(h, false)?;
    let _lock = storage::Lock::take(&h.state().join("guardian.lock"))?;
    if inactive(h) || !config(h)?.enabled {
        return Ok(json!({"running":false}));
    }
    let me = save_pid(h, "guardian")?;
    let mut early = 0u32;
    while !inactive(h) {
        if config(h).is_ok_and(|c| !c.enabled) {
            break;
        }
        let mut child = match spawn_role(h, "worker") {
            Ok(c) => c,
            Err(e) => {
                log(h, "spawn_failed", json!(e));
                early = early.saturating_add(1);
                let until = Instant::now() + Duration::from_secs(if early <= 2 { 2 } else { 120 });
                while Instant::now() < until && !inactive(h) && !config(h).is_ok_and(|c| !c.enabled)
                {
                    events.wait(until.saturating_duration_since(Instant::now()), None);
                }
                continue;
            }
        };
        let child_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
        let child_fd = if child_fd >= 0 {
            Some(unsafe { OwnedFd::from_raw_fd(child_fd as i32) })
        } else {
            None
        };
        let born = Instant::now();
        let mut stop_sent = false;
        let mut stopping = None;
        loop {
            if (inactive(h) || config(h).is_ok_and(|c| !c.enabled)) && !stop_sent {
                if let Ok(i) = process_identity(child.id()) {
                    let _ = signal_pid(&i, libc::SIGTERM);
                }
                stop_sent = true;
                stopping = Some(Instant::now());
            }
            if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
                log(
                    h,
                    "worker_exit",
                    json!({"code":s.code(),"requested":stop_sent,"error":if s.success(){None}else{storage::read(&h.state().join("worker.log"),16384).ok()}}),
                );
                break;
            }
            if stopping.is_some_and(|t| t.elapsed() > Duration::from_secs(25)) {
                let _ = child.kill();
                let _ = child.wait();
                log(h, "worker_stop_timeout", Value::Null);
                break;
            }
            events.wait(
                Duration::from_secs(if stopping.is_some() {
                    1
                } else if child_fd.is_some() {
                    120
                } else {
                    1
                }),
                child_fd.as_ref().map(AsRawFd::as_raw_fd),
            );
        }
        if inactive(h) || config(h).is_ok_and(|c| !c.enabled) {
            break;
        }
        let wait = if born.elapsed() < Duration::from_secs(2) && early < 2 {
            early += 1;
            2
        } else {
            early = 0;
            120
        };
        log(h, "watchdog_wait", json!({"seconds":wait}));
        let deadline = Instant::now() + Duration::from_secs(wait);
        while Instant::now() < deadline && !inactive(h) {
            events.wait(deadline.saturating_duration_since(Instant::now()), None);
        }
    }
    remove_pid(h, "guardian", &me);
    Ok(json!({"guardian":"stopped"}))
}
pub fn worker(h: &Hardware) -> Result<Value> {
    signals();
    let mut events = Events::new(h, true)?;
    let _lock = storage::Lock::take(&h.state().join("worker.lock"))?;
    h.control_namespace()?;
    let me = save_pid(h, "worker")?;
    let mut controller = Controller::load(h)?;
    h.prepare_masks()?;
    let mut c = config(h)?;
    let mut last_profile = String::new();
    let mut monitor: Option<Monitor> = None;
    let mut last_camera: Option<Usage> = None;
    let mut last_revision = c.revision;
    let mut controls: Option<ControlEvents> = None;
    let mut controls_retry = Instant::now();
    let mut controls_error: Option<String> = None;
    let mut schedule = Schedule::default();
    let mut last_plan_config: Option<Config> = None;
    let mut last_plan_targets = None;
    let mut ops = vec![];
    let mut statuses: Vec<control::BackendStatus> = vec![];
    let mut last_snapshot: Option<Value> = None;
    let mut last_published = Instant::now() - Duration::from_secs(120);
    let mut last_modes: Option<Value> = None;
    let mut detailed_state = None;
    let mut detailed_error: Option<String> = None;
    let mut last_cooling_health: Option<Value> = None;
    let mut charge = h
        .read("/sys/class/power_supply/battery/status")
        .unwrap_or_else(|_| "Unknown".into());
    let mut nodes = discovery::thermal(h);
    let mut discovered = Instant::now();
    log(
        h,
        "charge_listener",
        json!({"mode":if events.power_available(){"uevent"}else{"unavailable"},"error":events.power_error,"periodic_charge_reads":false}),
    );
    while !inactive(h) && c.enabled {
        let config_error = match config(h) {
            Ok(new) => {
                c = new;
                None
            }
            Err(e) => Some(e),
        };
        if !c.enabled {
            break;
        }
        let (profile, targets) = c.select(&charge);
        if detailed_state != Some(c.detailed_logging) {
            detailed_state = Some(c.detailed_logging);
            log(
                h,
                "detailed_logging_changed",
                json!({"enabled":c.detailed_logging}),
            );
            if c.detailed_logging {
                detailed_error = detail_log(h, "capture_started", json!({"config":c,"boot_id":controller.journal.boot_id,"device":device_info(h),"capabilities":h.capabilities(),"kernel":h.read("/proc/sys/kernel/osrelease").ok()})).err();
            } else {
                detailed_error = None;
            }
        }
        if c.revision != last_revision {
            log(h, "config_changed", json!({"revision":c.revision}));
            last_revision = c.revision;
        }
        if profile != last_profile {
            controller.stable = 0;
            log(
                h,
                "profile_changed",
                json!({"from":last_profile,"to":profile}),
            );
            last_profile = profile.into();
        }
        let smart = profile == "charging"
            && !c.effective(&charge, false).charge_engineer_policy
            && c.charge_horae_enabled
            && c.charge_horae_mode == HoraeMode::Smart;
        if smart && monitor.is_none() {
            match Monitor::start(h.clone()) {
                Ok(m) => monitor = Some(m),
                Err(e) => log(h, "camera_listener_failed", json!({"error":e})),
            }
        }
        if !smart {
            monitor = None;
            last_camera = None;
        }
        let camera = monitor.as_ref().map(Monitor::snapshot);
        if let Some(obs) = &camera {
            if last_camera != Some(obs.usage) {
                log(
                    h,
                    "camera_state",
                    json!({"usage":obs.usage,"reason":obs.reason}),
                );
                last_camera = Some(obs.usage);
            }
        }
        let allow = camera.as_ref().is_some_and(|o| o.permits_horae(now_ms()));
        // Camera recovery takes priority over discovery and other backend maintenance.
        if smart && !allow && controller.journal.entries.contains_key("horae") {
            let detected_ms = camera.as_ref().map(|o| o.sampled_ms);
            let result = controller.yield_horae(h);
            log(
                h,
                "horae_camera_restore",
                json!({"camera_sampled_ms":detected_ms,
                "service_state":h.service_state("horae").ok().flatten(),"backends":result}),
            );
        }
        let effective = c.effective(&charge, allow);
        controller.set_write_mode(c.write_mode);
        let event_mode = effective.enabled && c.write_mode == WriteMode::Event;
        if event_mode && controls.is_none() && Instant::now() >= controls_retry {
            match ControlEvents::new() {
                Ok(listener) => {
                    controls = Some(listener);
                    controls_error = None;
                }
                Err(error) => {
                    controls_error = Some(error);
                    controls_retry = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        if !event_mode {
            controls = None;
            controls_error = None;
        }
        let mut rediscovered = false;
        if effective.enabled && discovered.elapsed() >= Duration::from_secs(60) {
            h.refresh_capabilities();
            nodes = discovery::thermal(h);
            discovered = Instant::now();
            rediscovered = true;
            schedule.rediscover(
                !controls
                    .as_ref()
                    .is_some_and(ControlEvents::service_discovery_available),
            );
        }
        if rediscovered
            || last_plan_config.as_ref() != Some(&effective)
            || last_plan_targets != Some(targets)
        {
            ops = control::plan(h, &effective, targets, &nodes);
            schedule.set_plan(c.write_mode, &ops);
            last_plan_config = Some(effective.clone());
            last_plan_targets = Some(targets);
        }
        if let Some(listener) = &mut controls {
            listener.set_operations(h, &ops);
            schedule.notify(listener.reconnect(h));
            schedule.notify(listener.drain(h));
        }
        let cooling_health = controls.as_ref().map(ControlEvents::health).map(|health| json!({"ready":health["cooling"]["ready"],"error":health["cooling_error"],"generation":health["cooling_reconnections"]}));
        if last_cooling_health != cooling_health {
            log(
                h,
                "cooling_listener_changed",
                json!({"transport":cooling_health,"policy":"pure_event_when_all_sources_ready;local_periodic_fallback_on_loss"}),
            );
            last_cooling_health = cooling_health;
        }
        let supported = ops
            .iter()
            .filter(|o| {
                controls.as_ref().is_some_and(|l| l.supports(h, o))
                    && (!matches!(o.method, control::Method::Bind { .. })
                        || events.power_available())
            })
            .map(|o| o.id.clone())
            .collect::<BTreeSet<_>>();
        let due = schedule.due(
            now_ms(),
            controller.interval_secs() * 1000,
            supported,
            &statuses,
        );
        let previous_round = controller.rounds;
        statuses = controller.reconcile_selected(h, &ops, &due, || {
            smart
                && monitor
                    .as_ref()
                    .is_some_and(|m| !m.snapshot().permits_horae(now_ms()))
        });
        if effective.enabled
            && !effective.charge_engineer_policy
            && h.capabilities().shell_slots.is_none()
        {
            statuses.push(control::BackendStatus {
                id: "shell_temp_capability".into(),
                family: "shell".into(),
                target: "/proc/shell-temp".into(),
                desired: targets.battery.to_string(),
                state: "unverified".into(),
                detail: Some("shell_driver_unverified".into()),
            });
        }
        if c.detailed_logging && !due.is_empty() {
            let processed = now_ms();
            let records = due.iter().filter_map(|id| {
                let op = ops.iter().find(|o| &o.id == id)?;
                let state = statuses.iter().find(|s| &s.id == id);
                let receipts = schedule.receipts(id);
                let checked = controller.verification(id).map_or(processed, |v| v.checked_ms);
                let latency = receipts.and_then(|r| r.iter().filter_map(|e| e["received_ms"].as_u64()).min()).map(|ms| checked.saturating_sub(ms));
                Some(json!({"id":id,"target":op.target,"sensor_type":op.sensor_type,"desired":op.desired,
                    "trigger":schedule.pending_cause(id),"notifications":receipts,"notification_to_check_ms":latency,
                    "pure_event":schedule.is_event(id) && schedule.pending_cause(id)!=Some("retry") && state.is_some_and(|s|s.state=="applied"),"result":state,"verification":controller.verification(id)}))
            }).collect::<Vec<_>>();
            detailed_error = detail_log(h, "maintenance_decisions", json!({"round":controller.rounds,"profile":profile,"write_mode":c.write_mode,"operations":records})).err();
        }
        schedule.completed(&due, now_ms());
        schedule.set_repair_deadlines(controller.pending_repairs());
        if let Some(listener) = &mut controls {
            listener.observe_statuses(h, &statuses);
        }
        if smart
            && allow
            && monitor
                .as_ref()
                .is_some_and(|m| !m.snapshot().permits_horae(now_ms()))
        {
            statuses.retain(|s| s.id != "horae");
            statuses.extend(controller.yield_horae(h));
        }
        let failures = statuses
            .iter()
            .filter(|s| matches!(s.state.as_str(), "retrying" | "restore_pending"))
            .count();
        let interval = if profile == "idle" && failures == 0 {
            120
        } else {
            controller.interval_secs()
        };
        let backend_views = statuses
            .iter()
            .map(|s| {
                let mut v = json!(s);
                let op = ops.iter().find(|o| o.id == s.id);
                let retrying = matches!(s.state.as_str(), "retrying" | "restore_pending");
                let blocked = matches!(s.state.as_str(), "unverified" | "blocked");
                let repair_wait = s.state == "repair_wait";
                let event = schedule.is_event(&s.id) && !retrying;
                let dormant = matches!(
                    s.id.as_str(),
                    "omrg"
                        | "migt"
                        | control::ORMS
                        | crate::pps::ID
                        | crate::engineer::ID
                        | crate::power::ID
                ) && matches!(s.state.as_str(), "unavailable" | "unsupported");
                let hybrid = op.is_some_and(|o| controls.as_ref().is_some_and(|l| l.hybrid(o)));
                let verify =
                    c.write_mode == WriteMode::Event && op.is_some_and(control::can_verify);
                v["maintenance_mode"] = json!(if blocked {
                    "blocked"
                } else if dormant {
                    "dormant"
                } else if event {
                    "event"
                } else if hybrid {
                    "event_check"
                } else {
                    "loop"
                });
                v["periodic_interval_ms"] = if event || dormant || blocked {
                    Value::Null
                } else {
                    json!(controller.interval_secs() * 1000)
                };
                v["write_policy"] = json!(if dormant || blocked {
                    "none"
                } else if event || verify {
                    "on_drift"
                } else {
                    "periodic_write"
                });
                v["can_verify"] = json!(op.is_some_and(control::can_verify));
                if let Some(info) = controller.verification(&s.id) {
                    v.as_object_mut().unwrap().extend(
                        serde_json::to_value(info)
                            .unwrap()
                            .as_object()
                            .unwrap()
                            .clone(),
                    );
                }
                v["maintenance_reason"] = if c.write_mode == WriteMode::Event && !event {
                    json!(op.map(|o| {
                        if matches!(o.method, control::Method::Bind { .. })
                            && !events.power_available()
                        {
                            "设备拓扑事件监听暂不可用"
                        } else {
                            controls
                                .as_ref()
                                .map_or("事件监听尚未就绪", |l| l.reason(o))
                        }
                    }))
                } else {
                    Value::Null
                };
                if dormant {
                    v["maintenance_reason"] =
                        json!("目标不存在，等待重新发现；已有恢复记录继续保留");
                } else if verify && !event {
                    v["maintenance_reason"] = json!(if retrying {
                        "本节点正在重试；按原周期核验并修复，其他事件后端继续运行"
                    } else if hybrid {
                        "文件事件立即核对；保留原周期只读审计，发现漂移才写入"
                    } else {
                        "文件监听暂不可用；原周期只读审计及漂移修复继续运行"
                    });
                }
                v["event_source"] = if event {
                    json!(op.map(|o| if o.method == control::Method::Service {
                        "property_wait"
                    } else if o.family == "cooling" {
                        "thermal_tracepoint+thermal_netlink+inotify"
                    } else if o.target == control::BOUNCE {
                        "node_inotify"
                    } else {
                        "mount_poll+inotify"
                    }))
                } else {
                    Value::Null
                };
                if hybrid && !event {
                    v["event_source"] = json!("node_inotify");
                }
                if event && op.is_some_and(|o| o.family == "cooling") {
                    v["maintenance_reason"] = json!(
                        "通知触发回读并按需修复；正常时无周期核验，监听失效时局部恢复周期核验"
                    );
                } else if event && op.is_some_and(|o| o.target == control::BOUNCE) {
                    v["maintenance_reason"] = json!(
                        "文件事件触发回读并按需修复；正常时无周期核验，监听失效时局部恢复周期核验"
                    );
                }
                if repair_wait {
                    v["maintenance_reason"] =
                        json!("已检测漂移，等待修复窗口；截止时间到达后自动核验，无需新的外部事件");
                } else if blocked {
                    v["maintenance_reason"] =
                        json!("驱动槽位尚未验证，暂停 shell-temp 写入；其他后端独立运行");
                }
                if op.is_some_and(|o| o.method == control::Method::Shell) {
                    v["capability_verified"] = json!(true);
                    v["readback_semantics"] = json!("maximum_of_slots_not_individual_values");
                }
                v["last_maintenance_ms"] = json!(schedule.last_checked(&s.id));
                v["last_trigger"] = json!(schedule.last_trigger(&s.id));
                v
            })
            .collect::<Vec<_>>();
        let modes = json!({"configured":c.write_mode,"event_backends":backend_views.iter().filter(|b|b["maintenance_mode"]=="event").count(),
            "periodic_backends":backend_views.iter().filter(|b|b["maintenance_mode"]=="loop"||b["maintenance_mode"]=="event_check").count(),
            "hybrid_backends":backend_views.iter().filter(|b|b["maintenance_mode"]=="event_check").count(),
            "dormant_backends":backend_views.iter().filter(|b|b["maintenance_mode"]=="dormant").count()});
        if last_modes.as_ref() != Some(&modes) {
            log(h, "maintenance_mode_changed", modes.clone());
            last_modes = Some(modes);
        }
        let mut snapshot = json!({"schema":3,"version":crate::VERSION,"pid":me.pid,"start_ticks":me.start_ticks,"boot_id":controller.journal.boot_id,
            "phase":if failures>0{"partial"}else if statuses.iter().any(|s|s.state=="unverified"){"degraded"}else if statuses.iter().any(|s|s.state=="repair_wait"){"repair_wait"}else if profile=="idle"{"idle"}else{"running"},"sampled_ms":now_ms(),"profile":profile,"config":c,
            "camera":monitor.as_ref().map(Monitor::snapshot),"smart_horae":smart,
            "config_error":config_error,"charge_status":charge,"targets":targets,"round":controller.rounds,"interval_secs":interval,
            "charge_listener":if events.power_available(){"uevent"}else{"unavailable"},"charge_listener_error":events.power_error,
            "listeners":events.health(),"backends":backend_views,
            "node_inventory":nodes.iter().map(|n|json!({"path":n.path,"type":n.kind,"group":n.group})).collect::<Vec<_>>(),
            "logging":{"detailed_enabled":c.detailed_logging,"detail_error":detailed_error,"runtime_files":5,"runtime_file_bytes":262144,"detail_files":storage::DETAIL_LOG_BACKUPS+1,"detail_file_bytes":storage::DETAIL_LOG_FILE_BYTES},
            "write_mode":c.write_mode,"control_listeners":controls.as_ref().map(ControlEvents::health),"control_listener_error":controls_error,
            "counters":h.counters(),
            "device_capabilities":h.capabilities(),
            "pps_assist":crate::pps::status(h).ok(),
            "power_limit":crate::power::status(h).ok(),
            "heartbeat_interval_secs":120,
            "owned_count":controller.journal.entries.len(),"mount_count":controller.journal.entries.values().filter(|e|matches!(e.op.method,control::Method::Bind{..}|control::Method::Engineer)&&e.applied).count()});
        snapshot["counters"]["worker_cpu_ms"] = json!(worker_cpu_ms());
        snapshot["counters"].as_object_mut().unwrap().extend(
            controller
                .verification_counters()
                .as_object()
                .unwrap()
                .clone(),
        );
        // Compare semantic state separately from timestamps/diagnostic counters.
        let mut comparison = snapshot.clone();
        comparison.as_object_mut().unwrap().remove("sampled_ms");
        comparison.as_object_mut().unwrap().remove("counters");
        if let Some(listeners) = comparison["control_listeners"].as_object_mut() {
            listeners.remove("wakeups");
        }
        if last_snapshot.as_ref() != Some(&comparison)
            || last_published.elapsed() >= Duration::from_secs(120)
        {
            last_published = Instant::now();
            snapshot["sampled_ms"] = json!(now_ms());
            if let Err(error) = storage::json(&h.state().join("status.json"), &snapshot) {
                log(h, "status_write_failed", json!({"error":error}));
            } else {
                h.status_written();
                last_snapshot = Some(comparison);
            }
        }
        if controller.rounds != previous_round
            && (failures > 0 || controller.rounds == 1 || controller.rounds % 30 == 0)
        {
            log(
                h,
                "maintenance",
                json!({"round":controller.rounds,"profile":profile,"failures":failures,"due_backends":due.len(),
                    "write_mode":c.write_mode,"counters":h.counters(),"event_wakeups":controls.as_ref().map(|l|l.wakeups)}),
            );
        }
        let mut wait_ms = if profile == "idle" && failures == 0 {
            120_000
        } else {
            schedule.wait_ms(now_ms(), controller.interval_secs() * 1000)
        };
        if effective.enabled {
            wait_ms = wait_ms.min(
                Duration::from_secs(60)
                    .saturating_sub(discovered.elapsed())
                    .as_millis() as u64,
            );
        }
        if let Some(listener) = &controls {
            if let Some(retry) = listener.retry_in(h.fixture) {
                wait_ms = wait_ms.min(retry.as_millis() as u64);
            }
        } else if event_mode {
            wait_ms = wait_ms.min(
                controls_retry
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64,
            );
        }
        wait_ms = wait_ms.min(
            Duration::from_secs(120)
                .saturating_sub(last_published.elapsed())
                .as_millis() as u64,
        );
        let until = Instant::now() + Duration::from_millis(wait_ms);
        while Instant::now() < until && !inactive(h) {
            if RELOAD.swap(false, Ordering::Relaxed) {
                break;
            }
            if monitor
                .as_ref()
                .is_some_and(|m| m.snapshot().permits_horae(now_ms()) != allow)
            {
                break;
            }
            let wake = events.wait(
                until.saturating_duration_since(Instant::now()),
                controls.as_ref().map(ControlEvents::fd),
            );
            if wake.discovery {
                discovered = Instant::now() - Duration::from_secs(60);
            }
            if wake.power {
                // Startup and actual power events are the only charge-status reads.
                // A failed listener is surfaced; it never enables polling as a fallback.
                let next = if events.power_available() || h.fixture {
                    h.read("/sys/class/power_supply/battery/status")
                        .unwrap_or_else(|_| "Unknown".into())
                } else {
                    "Unknown".into()
                };
                if next != charge {
                    log(
                        h,
                        "charge_changed",
                        json!({"from":charge,"to":next,"source":"uevent"}),
                    );
                    charge = next;
                    break;
                }
            }
            if wake.changed || wake.discovery {
                break;
            }
        }
    }
    drop(controls);
    drop(monitor);
    let restored = controller.restore(h)?;
    storage::json(
        &h.state().join("status.json"),
        &json!({"schema":3,"version":crate::VERSION,"phase":"stopped","sampled_ms":now_ms(),"backends":restored,"owned_count":controller.journal.entries.len(),"config":c}),
    )?;
    remove_pid(h, "worker", &me);
    Ok(json!({"stopped":true,"remaining":controller.journal.entries.len()}))
}
pub fn stop(h: &Hardware) -> Result<Value> {
    let _lock = storage::Lock::take(&h.state().join("lifecycle.lock"))?;
    storage::atomic(&h.state().join("manual-stop"), b"manual\n")?;
    let config_result = change_enabled(h, false);
    for name in ["guardian", "worker"] {
        if let Some(i) = identity(h, name) {
            signal_pid(&i, libc::SIGTERM)?;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while ["guardian", "worker"]
        .iter()
        .any(|n| identity(h, n).is_some_and(|i| alive(&i)))
    {
        if Instant::now() > deadline {
            for name in ["worker", "guardian"] {
                if let Some(i) = identity(h, name) {
                    signal_pid(&i, libc::SIGKILL)?;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _worker = storage::Lock::take(&h.state().join("worker.lock"))?;
    h.control_namespace()?;
    let mut controller = Controller::load(h)?;
    let report = controller.restore(h)?;
    let remaining = controller.journal.entries.len();
    if remaining > 0 {
        return Err(format!("restore_pending:{remaining};{report:?}"));
    }
    Ok(json!({"stopped":true,"restored":true,"backends":report,"config_error":config_result.err()}))
}
// Presentation only: the OEM UI protocol can include keep/override states.
// Never use this field for power-limit control or infer PPS from USB_PD_PPS.
fn charging_protocol(h: &Hardware) -> Value {
    const OEM: &str = "/sys/class/oplus_chg/common/protocol_type";
    let raw = h.read(OEM).ok();
    let label = match raw.as_deref() {
        Some("1") => Some("VOOC"),
        Some("2") => Some("SVOOC"),
        Some("3") => Some("PD"),
        Some("4") => Some("QC"),
        Some("5") => Some("PPS"),
        Some("6") => Some("UFCS"),
        _ => None,
    };
    if let Some(label) = label {
        return json!({"label":label,"source":OEM,"raw":raw});
    }
    // Only the bracketed entry is active; the other entries list capabilities.
    let usb = h.read("/sys/class/power_supply/usb/usb_type").ok();
    let active = usb
        .as_deref()
        .and_then(|s| s.split_once('['))
        .and_then(|(_, s)| s.split_once(']'))
        .map(|(s, _)| s);
    let label = match active {
        Some("SDP" | "CDP") => "USB",
        Some("DCP") => "DCP",
        _ => "协议未知",
    };
    json!({"label":label,"source":"/sys/class/power_supply/usb/usb_type","raw":usb})
}
fn battery(h: &Hardware) -> Value {
    json!({"path":GAUGE,"raw":h.read(GAUGE).ok(),"temperature_c":h.read(GAUGE).ok().and_then(|s|s.parse::<f64>().ok()).map(|v|v/10.0),
        "status":h.read("/sys/class/power_supply/battery/status").unwrap_or_else(|_|"Unknown".into()),"protocol":charging_protocol(h),
        "note":"本模块不覆盖此电池接口；第三方覆盖与厂商单位仍需设备核对", "capacity":h.read("/sys/class/power_supply/battery/capacity").ok(),
        "current_raw":h.read("/sys/class/power_supply/battery/current_now").ok(),"voltage_raw":h.read("/sys/class/power_supply/battery/voltage_now").ok()})
}
fn node_views(h: &Hardware, c: &Controller) -> Vec<Value> {
    discovery::thermal(h).iter().map(|n|{
        let op=c.journal.entries.values().find(|e|e.op.target==n.path||n.emul_path.as_ref()==Some(&e.op.target));
        let raw=h.read(&n.path).ok();let temp_c=raw.as_ref().and_then(|s|s.parse::<f64>().ok()).and_then(|v|{
            if v == -273000.0{return None;}
            match n.group{
                discovery::Group::Shell|discovery::Group::Battery=>Some(v/1000.0),
                discovery::Group::Cpu|discovery::Group::Gpu|discovery::Group::Ddr=>Some(if v>0.0&&v<200.0{v}else if v>=200.0&&v<10000.0{v/10.0}else{v/1000.0}),
                _=>None,
            }
        });
        json!({"path":n.path,"type":n.kind,"group":n.group,"raw":raw,"temp_c":temp_c,"modified":op.is_some_and(|e|e.applied),
            "target_raw":op.map(|e|&e.op.desired),"method":op.map(|e|&e.op.method),"note":if op.is_some(){"接口读数，不标作真实温度"}else{"本模块未修改此接口"}})
    }).collect()
}
pub fn status(h: &Hardware) -> Result<Value> {
    status_view(h, true)
}
pub fn ui_status(h: &Hardware) -> Result<Value> {
    let mut view = status_view(h, false)?;
    view["device_info"] = device_info(h);
    Ok(view)
}
/// One initial payload avoids painting the overview before its readings arrive.
/// Later refreshes keep using the lightweight ui-status endpoint.
pub fn ui_initial(h: &Hardware) -> Result<Value> {
    match status_view(h, true) {
        Ok(mut view) => {
            view["device_info"] = device_info(h);
            Ok(view)
        }
        Err(error) => {
            let mut view = ui_status(h)?;
            view["thermal_error"] = json!(error);
            Ok(view)
        }
    }
}
fn status_view(h: &Hardware, thermal_readings: bool) -> Result<Value> {
    let mut snapshot: Option<Value> =
        storage::load::<Value>(&h.state().join("status.json"), 2_097_152)
            .ok()
            .filter(|s| s.is_object());
    // Presentation telemetry is sampled on demand, never by the background worker.
    if let Some(s) = snapshot.as_mut() {
        // Read the same mount view as the worker, then restore the caller's view
        // so export paths and the rest of this command retain their usual namespace.
        let caller = if h.fixture || !thermal_readings {
            None
        } else {
            Some(fs::File::open("/proc/self/ns/mnt").map_err(|e| e.to_string())?)
        };
        if thermal_readings {
            h.control_namespace()?;
        }
        s["battery"] = battery(h);
        if let Ok(controller) = thermal_readings.then(|| Controller::load(h)).transpose() {
            if let Some(controller) = controller {
                s["nodes"] = json!(node_views(h, &controller));
            }
        }
        s["telemetry_ms"] = json!(now_ms());
        if let Some(caller) = caller {
            if unsafe { libc::setns(caller.as_raw_fd(), libc::CLONE_NEWNS) } < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
    }
    let worker = identity(h, "worker").filter(alive);
    let worker_alive = worker.is_some();
    let guardian_alive = identity(h, "guardian").is_some_and(|i| alive(&i));
    let age = snapshot
        .as_ref()
        .and_then(|s| s["sampled_ms"].as_u64())
        .map(|t| now_ms().saturating_sub(t));
    let same_worker = worker.as_ref().is_some_and(|i| {
        snapshot.as_ref().is_some_and(|s| {
            s["pid"].as_u64() == Some(i.pid as u64)
                && s["start_ticks"].as_u64() == Some(i.start_ticks)
        })
    });
    let boot = h
        .read("/proc/sys/kernel/random/boot_id")
        .unwrap_or_else(|_| "fixture".into());
    let same_boot = snapshot
        .as_ref()
        .is_some_and(|s| s["boot_id"].as_str() == Some(boot.as_str()));
    Ok(
        json!({"schema":3,"version":crate::VERSION,"worker_alive":worker_alive,"guardian_alive":guardian_alive,"fresh":same_worker&&same_boot&&age.is_some_and(|v|v < snapshot.as_ref().and_then(|s|s["heartbeat_interval_secs"].as_u64().or_else(||s["interval_secs"].as_u64())).unwrap_or(3).saturating_mul(1000).saturating_add(15000)),
        "age_ms":age,"manual_stop":h.state().join("manual-stop").exists(),"status":snapshot,"config":config(h).ok(),
        "last_worker_error":if worker_alive{None}else{storage::read(&h.state().join("worker.log"),16384).ok()}}),
    )
}
pub fn device_info(h: &Hardware) -> Value {
    let model = h
        .prop("ro.product.model")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "未知型号".into());
    let display = h.prop("ro.build.display.id").unwrap_or_default();
    let brand = h.prop("ro.product.brand").unwrap_or_default();
    let rom = h.prop("ro.build.version.oplusrom").unwrap_or_default();
    let version = display
        .split('_')
        .find(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()) && v.contains('.'))
        .unwrap_or(rom.trim_start_matches('V'))
        .split('(')
        .next()
        .unwrap_or("");
    let label = if brand.eq_ignore_ascii_case("realme") {
        "realme UI"
    } else if brand.eq_ignore_ascii_case("oneplus") && !display.contains("CN") {
        "OxygenOS"
    } else {
        "ColorOS"
    };
    json!({"model":model,"oem_version":if version.is_empty(){"系统版本未知".into()}else{format!("{label} {version}")},"display_id":display})
}
pub fn diagnose(h: &Hardware) -> Result<Value> {
    Ok(
        json!({"version":crate::VERSION,"fixture":h.fixture,"status":status(h)?,"battery":battery(h),
        "device":{"model":h.prop("ro.product.model").ok(),"brand":h.prop("ro.product.brand").ok(),"sdk":h.prop("ro.build.version.sdk").ok()},
        "thermal":discovery::thermal(h),"cooling":discovery::cooling_nodes(h),"ownership":Controller::load(h)?.journal,
        "device_capabilities":h.capabilities(),
        "service_observations":(["thermal-engine",control::ORMS,"horae"].map(|name|json!({"service":name,"result":h.service_state(name)})))}),
    )
}
pub(crate) fn log_tail(h: &Hardware, name: &str, max: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let result = (|| -> std::io::Result<String> {
        let mut f = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(h.state().join(name))?;
        let meta = f.metadata()?;
        if !meta.is_file() {
            return Ok(String::new());
        }
        let offset = meta.len().saturating_sub(max as u64);
        f.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![];
        f.take(max as u64).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(if offset > 0 {
            text.split_once('\n')
                .map(|(_, s)| s.to_owned())
                .unwrap_or_default()
        } else {
            text
        })
    })();
    result.unwrap_or_default()
}
pub fn logs(h: &Hardware) -> Result<Value> {
    let mut entries = vec![];
    let names = std::iter::once("events.previous.jsonl".to_owned())
        .chain((1..=4).rev().map(|i| format!("events.jsonl.{i}")))
        .chain(std::iter::once("events.jsonl".into()));
    for name in names {
        for line in log_tail(h, &name, 65536).lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                entries.push(v);
            }
        }
    }
    if entries.len() > 100 {
        entries.drain(..entries.len() - 100);
    }
    Ok(
        json!({"entries":entries,"worker_error":log_tail(h,"worker.log",16384),"guardian_error":log_tail(h,"daemon.log",16384),"sampled_ms":now_ms()}),
    )
}
pub fn export_log(h: &Hardware) -> Result<Value> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let filename = format!(
        "{}-{stamp}-{}-{}.log",
        crate::ID,
        now_ms(),
        std::process::id()
    );
    let mut content = format!(
        "{} {}\nauthor={}\nmodule_id={}\nexport_unix_seconds={stamp}\n\n=== STATUS ===\n",
        crate::NAME,
        crate::VERSION,
        crate::AUTHOR,
        crate::ID
    );
    // Export saved diagnostics without a potentially slow thermal node scan.
    content.push_str(
        &serde_json::to_string_pretty(&status_view(h, false)?).map_err(|e| e.to_string())?,
    );
    for (stem, max, keep) in [
        ("events.jsonl", 262144, 4),
        (
            "detail.jsonl",
            storage::DETAIL_LOG_FILE_BYTES,
            storage::DETAIL_LOG_BACKUPS,
        ),
    ] {
        for i in (1..=keep).rev() {
            let name = format!("{stem}.{i}");
            content.push_str(&format!("\n\n=== {name} ===\n"));
            content.push_str(&log_tail(h, &name, max));
        }
    }
    for (name, max) in [
        ("events.previous.jsonl", 131072),
        ("events.jsonl", 262144),
        ("detail.jsonl", storage::DETAIL_LOG_FILE_BYTES),
        ("worker.log", 16384),
        ("camera-events.log", 16384),
        ("daemon.log", 16384),
        ("startup.log", 32768),
    ] {
        content.push_str(&format!("\n\n=== {name} ===\n"));
        content.push_str(&log_tail(h, name, max));
    }
    content.push('\n');
    publish_log(h, &filename, content, "chargeguard.log")
}
fn publish_log(h: &Hardware, filename: &str, content: String, saved_name: &str) -> Result<Value> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = h.state().join(saved_name);
    storage::atomic(&path, content.as_bytes())?;
    let mut public_error = None;
    let public_path = if h.exists("/sdcard/Download") {
        let p = h.path(&format!("/sdcard/Download/{filename}"));
        let mut created = false;
        let r = (|| -> std::io::Result<()> {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&p)?;
            created = true;
            f.write_all(content.as_bytes())?;
            f.sync_all()
        })();
        match r {
            Ok(()) => Some(p),
            Err(error) => {
                public_error = Some(error.to_string());
                if created {
                    let _ = fs::remove_file(&p);
                }
                None
            }
        }
    } else {
        public_error = Some("下载目录不可用".to_owned());
        None
    };
    // Large logs must not cross Android's JavascriptInterface/loadUrl bridge.
    Ok(
        json!({"path":path,"public_path":public_path,"public_error":public_error,"filename":filename,"bytes":content.len()}),
    )
}
pub fn hardware_from_environment() -> Result<Hardware> {
    #[cfg(all(feature = "fixtures", not(target_os = "android")))]
    {
        if let Ok(p) = std::env::var("CG_FIXTURE_ROOT") {
            let p = fs::canonicalize(p).map_err(|e| e.to_string())?;
            if p == std::path::Path::new("/") {
                return Err("fixture_root_cannot_be_system_root".into());
            }
            return Ok(Hardware::fixture(p));
        }
    }
    Hardware::production()
}
/// Read-only feature inventory. A positive LKM result is eligibility; it never
/// claims the helper was loaded or that charging performance was measured.
pub fn probe_capabilities(h: &Hardware) -> Value {
    let caps = h.capabilities();
    json!({"api":1,"module_version":crate::VERSION,"boot_id":h.read("/proc/sys/kernel/random/boot_id").ok(),
        "scope":"static_preflight_not_load_test","device_capabilities":caps,
        "thermal_nodes":discovery::thermal(h).len(),
        "engineer_policy":"runtime_battery_and_config_check_required"})
}
pub fn probe_install(h: &Hardware) -> Result<Value> {
    let report = probe_capabilities(h);
    let path = h.state().join("capabilities-install.json");
    storage::atomic(
        &path,
        &serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )?;
    let caps = h.capabilities();
    Ok(
        json!({"report":path,"pps":if caps.pps_verified {"预检通过，加载时继续校验"}else{"暂不支持；详见能力报告"},
        "power":if caps.power_verified {"预检通过，加载时继续校验"}else{"暂不支持；详见能力报告"},
        "charging_control_started":false}),
    )
}
pub fn execute(args: &[String]) -> Result<Value> {
    let verb = args.first().map(String::as_str).unwrap_or("status");
    if verb == "version" {
        return Ok(
            json!({"id":crate::ID,"name":crate::NAME,"author":crate::AUTHOR,"version":crate::VERSION,"android_bionic":cfg!(target_os="android"),"fixture_compiled":cfg!(feature="fixtures")}),
        );
    }
    let h = hardware_from_environment()?;
    if verb == "init-config" {
        init(&h)?;
        return Ok(json!({"configured":true,"schema":3}));
    }
    if matches!(verb, "daemon" | "worker" | "start" | "arm") {
        init(&h)?;
    }
    match verb {
        "service-events" => crate::maintenance_events::service_helper(),
        "daemon" => daemon(&h),
        "worker" => worker(&h),
        "start" | "arm" => start(&h),
        "stop" | "recover" | "pause" => stop(&h),
        "config" => Ok(json!(config(&h)?)),
        "configure-hex" => configure(&h, args.get(1).ok_or("missing_configuration")?),
        "status" => status(&h),
        "ui-status" => ui_status(&h),
        "ui-initial" => ui_initial(&h),
        "diagnose" => diagnose(&h),
        "probe-capabilities" => Ok(probe_capabilities(&h)),
        "probe-install" => probe_install(&h),
        "device-info" => Ok(device_info(&h)),
        "logs" => logs(&h),
        "open-repository" | "open-author" => crate::browser::open(verb),
        "thermal-nodes" => {
            h.control_namespace()?;
            Ok(json!({"nodes":node_views(&h,&Controller::load(&h)?),"sampled_ms":now_ms()}))
        }
        "export" => export_log(&h),
        _ => Err("unknown_command".into()),
    }
}
