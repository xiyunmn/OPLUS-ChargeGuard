//! Exclusive helper lease; only private votes are owned, never OEM clients.
use crate::{hardware::Hardware, Result};
use serde::Serialize;
use std::{collections::BTreeMap, fs::File, io::Write};
pub const ID: &str = "charge_power_limit";
pub const DEVICE: &str = "/dev/charge_guard_power";
pub const STATUS: &str = "/sys/module/charge_guard_power/parameters/status";
pub const DRIVER_ID: &str = "pjz110-power-205fb7eb-v1";
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub ready: bool,
    pub enabled: bool,
    pub session: bool,
    pub watts: u32,
    pub mask: u32,
    pub protocol: i32,
    pub voltage_mv: i32,
    pub cap_ma: i32,
    pub effective_ma: i32,
    pub pending: bool,
    pub state: String,
    pub error: i32,
}
pub fn parse_status(raw: &str) -> Result<Status> {
    let mut fields = BTreeMap::new();
    for field in raw.split_whitespace() {
        let (k, v) = field.split_once('=').ok_or("power_status_invalid")?;
        if fields.insert(k, v).is_some() {
            return Err("power_status_duplicate".into());
        }
    }
    if fields.len() != 14
        || fields.get("api") != Some(&"1")
        || fields.get("driver") != Some(&DRIVER_ID)
    {
        return Err("power_helper_identity_invalid".into());
    }
    let num = |k| -> Result<i32> {
        fields
            .get(k)
            .ok_or("power_status_missing")?
            .parse()
            .map_err(|_| "power_status_number".into())
    };
    let boolean = |k| -> Result<bool> {
        match num(k)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("power_status_bool".into()),
        }
    };
    let state = fields
        .get("state")
        .ok_or("power_status_missing")?
        .to_string();
    if !matches!(
        state.as_str(),
        "off"
            | "waiting_protocol"
            | "waiting_voltage"
            | "applied"
            | "data_error"
            | "restore_pending"
            | "retired"
            | "conflict"
            | "vote_rejected"
    ) {
        return Err("power_status_state_invalid".into());
    }
    let s = Status {
        ready: boolean("ready")?,
        enabled: boolean("enabled")?,
        session: boolean("session")?,
        watts: num("watts")? as u32,
        mask: num("mask")? as u32,
        protocol: num("protocol")?,
        voltage_mv: num("voltage_mv")?,
        cap_ma: num("cap_ma")?,
        effective_ma: num("effective_ma")?,
        pending: boolean("pending")?,
        state,
        error: num("error")?,
    };
    if !(20..=100).contains(&s.watts)
        || !(1..=15).contains(&s.mask)
        || s.protocol > 4
        || s.cap_ma < 0
    {
        return Err("power_status_range".into());
    }
    Ok(s)
}
pub fn status(h: &Hardware) -> Result<Status> {
    parse_status(&h.read(STATUS)?)
}
pub fn check_available(h: &Hardware) -> Result<()> {
    if h.present("/sys/module/charge_guard_power")? {
        let s = status(h)?;
        if s.session || s.enabled || s.pending {
            return Err("power_session_busy".into());
        }
    }
    Ok(())
}
pub fn settings(raw: &str) -> Result<(u64, u32, u32)> {
    let v = raw.split_whitespace().collect::<Vec<_>>();
    if v.len() != 3 {
        return Err("power_settings_invalid".into());
    }
    let r = v[0].parse::<u64>().map_err(|_| "power_revision_invalid")?;
    let w = v[1].parse::<u32>().map_err(|_| "power_watts_invalid")?;
    let m = v[2].parse::<u32>().map_err(|_| "power_mask_invalid")?;
    if r > 9_000_000_000_000 || !(20..=100).contains(&w) || !(1..=15).contains(&m) {
        return Err("power_settings_range".into());
    }
    Ok((r, w, m))
}
#[derive(Default)]
pub struct Session {
    file: Option<File>,
    desired: Option<String>,
    #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
    fixture: Option<Hardware>,
}
impl Session {
    pub fn apply(&mut self, h: &Hardware, desired: &str) -> Result<Status> {
        let (_, watts, mask) = settings(desired)?;
        if !h.capabilities().power_verified {
            return Err("power_firmware_not_supported".into());
        }
        crate::capabilities::confirm_loaded(h)?;
        if !h.present("/sys/module/charge_guard_power")? {
            load(h)?;
        }
        let before = status(h)?;
        if !before.ready {
            return Err("power_helper_retired".into());
        }
        if self.file.is_none() {
            #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
            if h.fixture {
                if self.fixture.is_none() {
                    if before.session || before.pending {
                        return Err("power_session_busy".into());
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
        if self.desired.as_deref() != Some(desired) {
            if let Some(file) = self.file.as_mut() {
                file.write_all(format!("1 {watts} {mask}\n").as_bytes())
                    .map_err(|e| format!("power_configure:{e}"))?;
            }
            #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
            if h.fixture {
                h.write(DEVICE, &format!("1 {watts} {mask}"))?;
                fixture_status(h, watts, mask, true, "waiting_protocol")?;
            }
            self.desired = Some(desired.into());
        }
        let current = status(h)?;
        if !current.session || !current.enabled || current.watts != watts || current.mask != mask {
            return Err("power_config_not_confirmed".into());
        }
        Ok(current)
    }
    pub fn restore(&mut self, h: &Hardware) -> Result<()> {
        if !h.present("/sys/module/charge_guard_power")? {
            self.file = None;
            self.desired = None;
            return Ok(());
        }
        let before = status(h)?;
        if self.file.is_none() {
            #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
            let fixture_owned = h.fixture && self.fixture.is_some();
            #[cfg(not(any(test, all(feature = "fixtures", not(target_os = "android")))))]
            let fixture_owned = false;
            if before.session && !fixture_owned {
                return Err("power_restore_foreign_session".into());
            }
            if before.pending || before.enabled {
                if !h.fixture {
                    self.file = Some(open(h)?);
                }
            }
        }
        if let Some(f) = self.file.as_mut() {
            f.write_all(b"0\n")
                .map_err(|e| format!("power_restore:{e}"))?;
        }
        #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
        if h.fixture {
            h.write(DEVICE, "0")?;
            fixture_status(h, before.watts, before.mask, false, "off")?;
            self.fixture = None;
        }
        let s = status(h)?;
        if s.enabled || s.pending {
            return Err("power_restore_not_confirmed".into());
        }
        self.file = None;
        self.desired = None;
        unload(h)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        // Kernel release() withdraws votes on normal exit and SIGKILL alike.
        self.file = None;
        #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
        if let Some(h) = self.fixture.take() {
            let _ = fixture_status(&h, 50, 1, false, "off");
        }
    }
}
fn open(h: &Hardware) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(h.path(DEVICE))
        .map_err(|e| format!("power_session_open:{e}"))
}
#[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
fn fixture_status(h: &Hardware, w: u32, m: u32, on: bool, state: &str) -> Result<()> {
    std::fs::write(h.path(STATUS),format!("api=1 driver={DRIVER_ID} ready=1 enabled={} session={} watts={w} mask={m} protocol=0 voltage_mv=0 cap_ma=0 effective_ma=0 pending=0 state={state} error=0\n",on as u8,on as u8)).map_err(|e|e.to_string())
}
fn load(h: &Hardware) -> Result<()> {
    #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
    if h.fixture {
        std::fs::create_dir_all(h.path("/sys/module/charge_guard_power/parameters"))
            .map_err(|e| e.to_string())?;
        std::fs::create_dir_all(h.path("/dev")).map_err(|e| e.to_string())?;
        std::fs::write(h.path(DEVICE), "").map_err(|e| e.to_string())?;
        h.record(serde_json::json!({"kind":"power_lkm_load"}));
        return fixture_status(h, 50, 1, false, "off");
    }
    #[cfg(target_os = "android")]
    {
        use std::os::fd::AsRawFd;
        let f = File::open(h.path(&format!("{}/bin/charge_guard_power.ko", crate::MODULE)))
            .map_err(|e| e.to_string())?;
        if unsafe { libc::syscall(libc::SYS_finit_module, f.as_raw_fd(), b"\0".as_ptr(), 0) } != 0 {
            return Err(format!("power_load:{}", std::io::Error::last_os_error()));
        }
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    Err("power_requires_android".into())
}
fn unload(h: &Hardware) -> Result<()> {
    let _ = h;
    #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
    if h.fixture {
        std::fs::remove_dir_all(h.path("/sys/module/charge_guard_power"))
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(h.path(DEVICE));
        return Ok(());
    }
    #[cfg(target_os = "android")]
    {
        if unsafe {
            libc::syscall(
                libc::SYS_delete_module,
                b"charge_guard_power\0".as_ptr(),
                libc::O_NONBLOCK,
            )
        } != 0
        {
            return Err(format!("power_unload:{}", std::io::Error::last_os_error()));
        }
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    Err("power_requires_android".into())
}
