//! Battery telemetry has OEM units; USB current and current-limit votes are not
//! substitutes. This profile describes the OPLUS GKI battery supply contract.
use crate::{
    hardware::Hardware,
    statistics::{ChargeState, Readings},
    storage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

const BATTERY: &str = "/sys/class/power_supply/battery";
#[derive(Clone, Serialize, Deserialize)]
struct Measurement {
    schema: u32,
    boot_id: String,
    profile: String,
    power_available: bool,
    cell_count: Option<u32>,
    cell_count_sources: Vec<String>,
    voltage_scope: String,
    raw_current_unit: Option<String>,
    raw_voltage_unit: Option<String>,
    current_sign: String,
    raw_current_sign: Option<String>,
    power_method: Option<String>,
    current_path: String,
    voltage_path: String,
    note: String,
}
static PROFILE: OnceLock<Mutex<Option<(PathBuf, Measurement)>>> = OnceLock::new();

fn detect(h: &Hardware, boot_id: String) -> Measurement {
    // These names identify the implemented supply interface, not a handset or
    // ROM version. battery_psy_get_prop returns gauge mA and cell mV * 1000.
    let oplus = matches!(
        h.read(&format!("{BATTERY}/model_name")).ok().as_deref(),
        Some("oplus battery" | "uncertified battery")
    ) && h.exists("/sys/bus/platform/drivers/oplus_chg_gki");
    let mut counts = BTreeSet::new();
    let mut sources = Vec::new();
    if oplus {
        // Inspect instantiated, bound devices only. An entire DT contains
        // alternative (inactive) gauge configurations and cannot establish the
        // actual cell count. Conflicting active counts stay unknown.
        for bus in ["i2c", "platform"] {
            if let Ok(devices) = fs::read_dir(h.path(&format!("/sys/bus/{bus}/devices"))) {
                for device in devices.flatten() {
                    if !device.path().join("driver").exists() {
                        continue;
                    }
                    let property = device.path().join("of_node/oplus,batt_num");
                    if let Ok(bytes) = fs::read(&property) {
                        if let Ok(word) = <[u8; 4]>::try_from(bytes.as_slice()) {
                            let count = u32::from_be_bytes(word);
                            if (1..=4).contains(&count) {
                                counts.insert(count);
                                sources.push(
                                    property
                                        .strip_prefix(&h.root)
                                        .unwrap_or(&property)
                                        .to_string_lossy()
                                        .into_owned(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    let cell_count = (counts.len() == 1).then(|| *counts.first().unwrap());
    let power_available = oplus && cell_count.is_some();
    Measurement {
        schema: 1,
        boot_id,
        profile: if oplus { "oplus_gki" } else { "unverified" }.into(),
        power_available,
        cell_count,
        cell_count_sources: sources,
        voltage_scope: if oplus {
            "representative_cell"
        } else {
            "unverified"
        }
        .into(),
        raw_current_unit: oplus.then(|| "mA".into()),
        raw_voltage_unit: oplus.then(|| "uV".into()),
        current_sign: "positive_into_battery".into(),
        raw_current_sign: oplus.then(|| "negative_into_battery".into()),
        power_method: power_available.then(|| "representative_cell_times_cell_count".into()),
        current_path: format!("{BATTERY}/current_now"),
        voltage_path: format!("{BATTERY}/voltage_now"),
        note: match (oplus, cell_count) {
            (true, Some(2)) => "电池端净功率 · 双芯折算",
            (true, Some(1)) => "电池端净功率 · 单芯",
            (true, Some(_)) => "电池端净功率 · 按电芯数折算",
            (true, None) => "电芯数量未确认，功率暂不可用",
            _ => "电流与电压口径未确认，保留原始读数",
        }
        .into(),
    }
}

fn profile(h: &Hardware) -> Measurement {
    let mut cache = PROFILE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((root, value)) = &*cache {
        if root == &h.root {
            return value.clone();
        }
    }
    let boot = h
        .read("/proc/sys/kernel/random/boot_id")
        .unwrap_or_default();
    let path = h.state().join("statistics/measurement.json");
    let value = storage::load::<Measurement>(&path, 32768)
        .ok()
        .filter(|v| v.schema == 1 && !boot.is_empty() && v.boot_id == boot)
        .unwrap_or_else(|| detect(h, boot));
    // Cache is optional: a telemetry cache failure must not affect controls.
    let _ = storage::json(&path, &value);
    *cache = Some((h.root.clone(), value.clone()));
    value
}

pub fn description(h: &Hardware) -> Value {
    let mut value = serde_json::to_value(profile(h)).unwrap();
    value.as_object_mut().unwrap().remove("boot_id");
    value
}

pub fn readings(h: &Hardware) -> Readings {
    let profile = profile(h);
    let current_raw = h.read(&profile.current_path).ok();
    let voltage_raw = h.read(&profile.voltage_path).ok();
    let number = |s: &Option<String>| {
        s.as_ref()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
    };
    let known = profile.profile == "oplus_gki";
    let current_ma = known.then(|| number(&current_raw).map(|v| -v)).flatten();
    let voltage_mv = known
        .then(|| {
            number(&voltage_raw)
                .filter(|v| *v > 0.0)
                .map(|v| v / 1000.0)
        })
        .flatten();
    let net_power_w = current_ma
        .zip(voltage_mv)
        .zip(profile.cell_count)
        .map(|((i, v), n)| i * v * f64::from(n) / 1_000_000.0);
    Readings {
        soc: number(&h.read(&format!("{BATTERY}/capacity")).ok())
            .filter(|v| (0.0..=100.0).contains(v)),
        temp_c: number(&h.read(&format!("{BATTERY}/temp")).ok()).map(|v| v / 10.0),
        current_ma,
        voltage_mv,
        net_power_w,
        current_raw,
        voltage_raw,
        protocol: crate::runtime::charging_protocol(h)["label"]
            .as_str()
            .map(str::to_owned),
    }
}

pub fn charge_state(h: &Hardware, status: &str) -> ChargeState {
    let supply = |name: &str| -> Option<bool> {
        let base = format!("/sys/class/power_supply/{name}");
        if !h.exists(&base) {
            return Some(false);
        }
        for field in ["present", "online"] {
            if let Ok(value) = h.read(&format!("{base}/{field}")) {
                match value.as_str() {
                    "0" => return Some(false),
                    "1" => return Some(true),
                    _ => (),
                }
            }
        }
        None
    };
    let usb = supply("usb");
    let wireless = supply("wireless");
    let connected = if usb == Some(true) || wireless == Some(true) {
        Some(true)
    } else if usb == Some(false) && wireless == Some(false) {
        Some(false)
    } else {
        None
    };
    ChargeState {
        connected,
        status: status.into(),
        protocol: crate::runtime::charging_protocol(h)["label"]
            .as_str()
            .map(str::to_owned),
        source: if usb == Some(true) {
            Some("usb".into())
        } else if wireless == Some(true) {
            Some("wireless".into())
        } else {
            None
        },
    }
}

pub fn context(h: &Hardware) -> Value {
    let mut device = crate::runtime::device_info(h);
    for (key, property) in [
        ("fingerprint", "ro.build.fingerprint"),
        ("android_release", "ro.build.version.release"),
        ("sdk", "ro.build.version.sdk"),
        ("build_display", "ro.build.display.id"),
    ] {
        device[key] = json!(h.prop(property).ok());
    }
    device["kernel"] = json!(h.read("/proc/sys/kernel/osrelease").ok());
    device["module_version"] = json!(crate::VERSION);
    device
}
