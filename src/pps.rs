//! A narrowly scoped optional LKM; never writes OEM votes, PDOs or debug sensors.
use crate::{hardware::Hardware, Result};
use serde::Serialize;
use std::collections::BTreeMap;

pub const ID: &str = "pps_status_assist";
pub const MODULE_NAME: &str = "charge_guard_pps";
pub const ENABLED: &str = "/sys/module/charge_guard_pps/parameters/enabled";
pub const STATUS: &str = "/sys/module/charge_guard_pps/parameters/status";
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
    })
}

pub fn status(h: &Hardware) -> Result<Status> {
    parse_status(&h.read(STATUS)?)
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
    if !h.present("/sys/module/charge_guard_pps")? {
        load(h)?;
    }
    let s = status(h)?;
    if !s.ready || s.retiring {
        return Err("pps_driver_retired_or_not_ready".into());
    }
    Ok(())
}

pub fn restore(h: &Hardware, original: Option<&str>) -> Result<()> {
    let value = original
        .filter(|v| matches!(*v, "0" | "1"))
        .ok_or("pps_original_invalid")?;
    if !h.present("/sys/module/charge_guard_pps")? {
        return if value == "0" {
            Ok(())
        } else {
            Err("pps_restore_module_missing".into())
        };
    }
    status(h)?;
    h.write(ENABLED, value)?;
    let s = status(h)?;
    if h.read(ENABLED)? != value || (value == "0" && (s.enabled || s.applied)) {
        return Err("pps_restore_not_confirmed".into());
    }
    // Keep a pre-existing enabled helper intact; only unload after restoring off.
    if value == "0" {
        unload(h)?;
    }
    Ok(())
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
