//! Thermal selectors recovered from isolated observations of the original module.
use crate::hardware::Hardware;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    Battery,
    Cpu,
    Gpu,
    Ddr,
    Shell,
    Other,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub path: String,
    pub kind: String,
    pub group: Group,
    pub emul_path: Option<String>,
}
pub fn numeric(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .is_some_and(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
}
pub fn classify(kind: &str) -> Group {
    let k = kind.to_ascii_lowercase();
    if k == "battery" {
        Group::Battery
    } else if k.starts_with("shell")
        || matches!(k.as_str(), "batt-therm" | "xo-therm" | "wlan-therm")
    {
        Group::Shell
    } else if ["cpu-", "cpu_", "cpuss-", "aoss-"]
        .iter()
        .any(|p| k.starts_with(p))
        || matches!(k.as_str(), "socd" | "soc_max")
    {
        Group::Cpu
    } else if k.starts_with("gpu") || k == "kgsl" {
        Group::Gpu
    } else if k == "ddr" || k.starts_with("ddr-") || k.starts_with("dram") {
        Group::Ddr
    } else {
        Group::Other
    }
}
pub fn cooling(kind: &str) -> bool {
    kind.starts_with("cpufreq-cpu") || kind.starts_with("cpu-cluster")
}
pub fn thermal(h: &Hardware) -> Vec<Node> {
    let mut out = vec![];
    let mut seen = BTreeSet::new();
    for base in ["/sys/class/thermal", "/sys/devices/virtual/thermal"] {
        for name in h.entries(base) {
            if !numeric(&name, "thermal_zone") {
                continue;
            }
            let p = format!("{base}/{name}");
            let Ok(kind) = h.read(&format!("{p}/type")) else {
                continue;
            };
            let path = format!("{p}/temp");
            let id = h.canonical(&path).unwrap_or_else(|_| path.clone());
            if !seen.insert(id) {
                continue;
            }
            let emul = format!("{p}/emul_temp");
            out.push(Node {
                path,
                group: classify(&kind),
                kind,
                emul_path: h.exists(&emul).then_some(emul),
            });
        }
    }
    out
}
pub fn cooling_nodes(h: &Hardware) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut seen = BTreeSet::new();
    for base in ["/sys/class/thermal", "/sys/devices/virtual/thermal"] {
        for name in h.entries(base) {
            if !numeric(&name, "cooling_device") {
                continue;
            }
            let p = format!("{base}/{name}");
            let Ok(kind) = h.read(&format!("{p}/type")) else {
                continue;
            };
            let path = format!("{p}/cur_state");
            let id = h.canonical(&path).unwrap_or_else(|_| path.clone());
            if cooling(&kind) && seen.insert(id) {
                out.push((path, kind));
            }
        }
    }
    out
}
pub fn mask_raw(target_mc: i32, observed: i32) -> i32 {
    if observed > 0 && observed < 200 {
        target_mc / 1000
    } else if observed >= 200 && observed < 10000 {
        target_mc / 100
    } else {
        target_mc
    }
}
