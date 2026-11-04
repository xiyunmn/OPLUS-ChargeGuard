//! An exclusive lease for the OEM shared charging-screen view.
use crate::{hardware::Hardware, Result};
use serde::Serialize;
use std::{collections::BTreeMap, fs::File, io::Write};

pub const ID: &str = "charge_screen_fast";
pub const DEVICE: &str = "/dev/charge_guard_screen";
pub const STATUS: &str = "/sys/module/charge_guard_screen/parameters/status";
pub const DRIVER_ID: &str = "charge-guard-screen-v1";
const MODULE_PATH: &str = "/sys/module/charge_guard_screen";

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub ready: bool,
    pub enabled: bool,
    pub session: bool,
    pub active: bool,
    pub pending: bool,
    pub original_led: i32,
    pub effective_led: i32,
    pub observations: u64,
    pub publications: u64,
    pub state: String,
    pub error: i32,
}

pub fn parse_status(raw: &str) -> Result<Status> {
    let mut fields = BTreeMap::new();
    for field in raw.split_whitespace() {
        let (k, v) = field.split_once('=').ok_or("screen_status_invalid")?;
        if fields.insert(k, v).is_some() {
            return Err("screen_status_duplicate".into());
        }
    }
    if fields.len() != 13
        || fields.get("api") != Some(&"1")
        || fields.get("driver") != Some(&DRIVER_ID)
    {
        return Err("screen_helper_identity_invalid".into());
    }
    let value = |key| -> Result<&str> {
        fields
            .get(key)
            .copied()
            .ok_or_else(|| "screen_status_missing".into())
    };
    let number = |key| -> Result<i32> {
        value(key)?
            .parse()
            .map_err(|_| "screen_status_number".into())
    };
    let boolean = |key| -> Result<bool> {
        match number(key)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("screen_status_bool".into()),
        }
    };
    let counter = |key| -> Result<u64> {
        value(key)?
            .parse()
            .map_err(|_| "screen_status_counter".into())
    };
    let s = Status {
        ready: boolean("ready")?,
        enabled: boolean("enabled")?,
        session: boolean("session")?,
        active: boolean("active")?,
        pending: boolean("pending")?,
        original_led: number("original_led")?,
        effective_led: number("effective_led")?,
        observations: counter("observations")?,
        publications: counter("publications")?,
        state: value("state")?.into(),
        error: number("error")?,
    };
    if !(-1..=1).contains(&s.original_led)
        || !(-1..=1).contains(&s.effective_led)
        || !matches!(
            s.state.as_str(),
            "off"
                | "waiting_charge"
                | "applied"
                | "data_error"
                | "conflict"
                | "restore_pending"
                | "retired"
        )
    {
        return Err("screen_status_fields".into());
    }
    Ok(s)
}

pub fn status(h: &Hardware) -> Result<Status> {
    parse_status(&h.read(STATUS)?)
}

pub fn message(s: &Status) -> &'static str {
    match s.state.as_str() {
        "restore_pending" => "恢复未完成，恢复记录已保留",
        "applied" => "已应用息屏充电判断；实际电流仍受原厂约束",
        "waiting_charge" => "等待有线充电",
        "conflict" => "存在控制冲突，息屏充电判断未生效",
        "data_error" => "充电状态更新失败，请查看后端详情",
        "retired" => "原厂充电服务已退出，辅助已停止",
        _ => "已关闭",
    }
}

pub fn failure_state(error: &str) -> &'static str {
    if matches!(error, "screen_session_busy" | "screen_restore_foreign_session")
        || error.contains(":conflict:")
    {
        "conflict"
    } else if error == "screen_firmware_not_supported" {
        "unsupported"
    } else if error == "screen_helper_retired" || error.contains(":retired:") {
        "retired"
    } else {
        "data_error"
    }
}

pub fn check_available(h: &Hardware) -> Result<()> {
    if h.present(MODULE_PATH)? {
        let s = status(h)?;
        if s.session || s.enabled || s.active || s.pending {
            return Err("screen_session_busy".into());
        }
    }
    Ok(())
}

#[derive(Default)]
pub struct Session {
    file: Option<File>,
    attempted: Option<String>,
    failure: Option<String>,
    #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
    fixture: Option<Hardware>,
}

impl Session {
    pub fn apply(&mut self, h: &Hardware, revision: &str) -> Result<Status> {
        if self.failure.is_some() {
            // Restore an unsuccessful activation before accepting another one.
            // Reconciliation may retry recovery, never the failed activation.
            self.restore(h)?;
            if self.attempted.as_deref() == Some(revision) {
                return Err(self.failure.clone().unwrap());
            }
            self.failure = None;
        }
        if !h.capabilities().screen_verified {
            return Err("screen_firmware_not_supported".into());
        }
        crate::capabilities::confirm_loaded(h)?;
        if !h.present(MODULE_PATH)? {
            load(h)?;
        }
        let before = status(h)?;
        if before.pending {
            return Err("screen_restore_pending".into());
        }
        if !before.ready {
            return Err("screen_helper_retired".into());
        }
        if self.file.is_none() {
            #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
            if h.fixture {
                if self.fixture.is_none() {
                    if before.session {
                        return Err("screen_session_busy".into());
                    }
                    self.fixture = Some(h.clone());
                }
            } else {
                self.file = Some(open(h)?);
            }
            #[cfg(not(any(test, all(feature = "fixtures", not(target_os = "android")))))]
            {
                self.file = Some(open(h)?);
            }
        }
        let result = (|| -> Result<Status> {
            if self.attempted.as_deref() != Some(revision) {
                // An uncertain write result still counts as this revision's attempt.
                self.attempted = Some(revision.into());
                if let Some(file) = self.file.as_mut() {
                    file.write_all(b"1\n")
                        .map_err(|e| format!("screen_enable:{e}"))?;
                }
                #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
                if h.fixture {
                    h.write(DEVICE, "1")?;
                    fixture_status(h, true, "waiting_charge")?;
                }
            }
            let current = status(h)?;
            if !current.session
                || !current.enabled
                || current.pending
                || (current.state == "applied" && (!current.active || current.effective_led != 0))
            {
                return Err(format!(
                    "screen_enable_not_confirmed:{}:{}",
                    current.state, current.error
                ));
            }
            Ok(current)
        })();
        if let Err(error) = &result {
            self.failure = Some(error.clone());
            if let Err(recovery) = self.restore(h) {
                return Err(format!("{error};restore:{recovery}"));
            }
        }
        result
    }

    pub fn restore(&mut self, h: &Hardware) -> Result<()> {
        if !h.present(MODULE_PATH)? {
            self.file = None;
            if self.failure.is_none() {
                self.attempted = None;
            }
            return Ok(());
        }
        let before = status(h)?;
        if self.file.is_none() {
            #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
            let fixture_owned = h.fixture && self.fixture.is_some();
            #[cfg(not(any(test, all(feature = "fixtures", not(target_os = "android")))))]
            let fixture_owned = false;
            if before.session && !fixture_owned {
                return Err("screen_restore_foreign_session".into());
            }
            if (before.pending || before.enabled || before.active) && !h.fixture {
                self.file = Some(open(h)?);
            }
        }
        if let Some(file) = self.file.as_mut() {
            file.write_all(b"0\n")
                .map_err(|e| format!("screen_restore:{e}"))?;
        }
        #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
        if h.fixture {
            h.write(DEVICE, "0")?;
            fixture_status(h, false, "off")?;
            self.fixture = None;
        }
        let current = status(h)?;
        if current.enabled || current.active || current.pending {
            return Err("screen_restore_not_confirmed".into());
        }
        self.file = None;
        if self.failure.is_none() {
            self.attempted = None;
        }
        unload(h)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Native release republishes the real state when the process exits.
        self.file = None;
        #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
        if let Some(h) = self.fixture.take() {
            if h.write(DEVICE, "0").is_ok() {
                let _ = fixture_status(&h, false, "off");
            }
        }
    }
}

fn open(h: &Hardware) -> Result<File> {
    open_device(&h.path(DEVICE), std::time::Duration::from_secs(1)).map_err(|e| {
        if e.raw_os_error() == Some(libc::EBUSY) {
            "screen_session_busy".into()
        } else {
            format!("screen_control_open:{e}")
        }
    })
}

fn open_device(path: &std::path::Path, timeout: std::time::Duration) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let started = std::time::Instant::now();
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
        {
            // Android ueventd creates the misc node asynchronously after
            // finit_module returns. Only wait for that specific missing node;
            // permission, ownership and driver errors must fail immediately.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && started.elapsed() < timeout => {
                std::thread::sleep(
                    std::time::Duration::from_millis(20)
                        .min(timeout.saturating_sub(started.elapsed())),
                );
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;
    use std::{fs, time::Duration};

    #[test]
    fn waits_for_asynchronous_device_creation() {
        let path = std::env::temp_dir().join(format!("cg-screen-node-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let node = path.clone();
        let creator = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            fs::write(node, "").unwrap();
        });
        let opened = open_device(&path, Duration::from_secs(1));
        creator.join().unwrap();
        fs::remove_file(path).unwrap();
        assert!(opened.is_ok());
    }
}

#[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
fn fixture_status(h: &Hardware, on: bool, state: &str) -> Result<()> {
    std::fs::write(h.path(STATUS),format!("api=1 driver={DRIVER_ID} ready=1 enabled={} session={} active=0 pending=0 original_led=1 effective_led=1 observations=0 publications=0 state={state} error=0\n",on as u8,on as u8)).map_err(|e|e.to_string())
}

fn load(h: &Hardware) -> Result<()> {
    #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
    if h.fixture {
        std::fs::create_dir_all(h.path(&format!("{MODULE_PATH}/parameters")))
            .map_err(|e| e.to_string())?;
        std::fs::create_dir_all(h.path("/dev")).map_err(|e| e.to_string())?;
        std::fs::write(h.path(DEVICE), "").map_err(|e| e.to_string())?;
        h.record(serde_json::json!({"kind":"screen_lkm_load"}));
        return fixture_status(h, false, "off");
    }
    #[cfg(target_os = "android")]
    {
        use std::os::fd::AsRawFd;
        let file = File::open(h.path(&format!("{}/bin/charge_guard_screen.ko", crate::MODULE)))
            .map_err(|e| e.to_string())?;
        if unsafe { libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), b"\0".as_ptr(), 0) }
            != 0
        {
            return Err(format!("screen_load:{}", std::io::Error::last_os_error()));
        }
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    Err("screen_requires_android".into())
}

fn unload(h: &Hardware) -> Result<()> {
    let _ = h;
    #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
    if h.fixture {
        std::fs::remove_dir_all(h.path(MODULE_PATH)).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(h.path(DEVICE));
        return Ok(());
    }
    #[cfg(target_os = "android")]
    {
        if unsafe {
            libc::syscall(
                libc::SYS_delete_module,
                b"charge_guard_screen\0".as_ptr(),
                libc::O_NONBLOCK,
            )
        } != 0
        {
            return Err(format!("screen_unload:{}", std::io::Error::last_os_error()));
        }
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    Err("screen_requires_android".into())
}
