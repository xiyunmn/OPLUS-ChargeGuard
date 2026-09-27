//! Independent, idempotent control backends with recovery ownership, not drift gates.
use crate::{
    config::{Config, Temperatures, WriteMode},
    discovery::{self, Group, Node},
    hardware::{Hardware, MASKS},
    storage, Result,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::MetadataExt,
};
pub const GAME: &str = "/proc/game_opt/disable_cpufreq_limit";
pub const BOUNCE: &str = "/sys/module/cpufreq_bouncing/parameters/enable";
pub const OMRG: &str = "/sys/devices/platform/soc/soc:oplus-omrg/oplus-omrg0/ruler_enable";
pub const MIGT: &str = "/sys/module/migt/parameters/glk_freq_limit_walt";
pub const ORMS: &str = "vendor.oplus.ormsHalService-aidl-defaults";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Method {
    Write,
    Shell,
    Emulation,
    Bind { source: String },
    Service,
    Pps,
    Engineer,
    Power,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub family: String,
    pub target: String,
    pub desired: String,
    pub method: Method,
    pub reset: Option<String>,
    pub sensor_type: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owned {
    pub op: Operation,
    pub original: Option<String>,
    pub applied: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub schema: u32,
    pub boot_id: String,
    pub entries: BTreeMap<String, Owned>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendStatus {
    pub id: String,
    pub family: String,
    pub target: String,
    pub desired: String,
    pub state: String,
    pub detail: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Verification {
    pub observed_before: Option<String>,
    pub observed_after: Option<String>,
    pub checked_ms: u64,
    pub write_performed: bool,
    pub verification: String,
    pub verification_checks: u64,
    pub repair_writes: u64,
    pub skipped_writes: u64,
    pub retry_after_ms: Option<u64>,
    pub last_write_error: Option<String>,
}
/// State-bearing interfaces, excluding masked game_opt displays and shell/emulation.
pub fn can_verify(op: &Operation) -> bool {
    op.method == Method::Pps
        || op.method == Method::Write
            && (op.target == BOUNCE
                || (op.family == "cooling" && op.target.ends_with("/cur_state")))
}
pub struct Controller {
    pub journal: Journal,
    pub rounds: u64,
    pub stable: u32,
    power: crate::power::Session,
    persisted: RefCell<Option<Journal>>,
    persisted_stamp: RefCell<Option<[u64; 10]>>,
    statuses: BTreeMap<String, BackendStatus>,
    verify_nodes: bool,
    verification: BTreeMap<String, Verification>,
    repair_after: BTreeMap<String, u64>,
}
pub fn mask_source(path: &str) -> String {
    format!("{MASKS}/mask_{:x}", Sha256::digest(path.as_bytes()))
}
fn temperature_input(op: &Operation) -> bool {
    matches!(
        op.method,
        Method::Shell | Method::Emulation | Method::Bind { .. }
    )
}
fn write(
    id: &str,
    family: &str,
    path: &str,
    value: String,
    reset: Option<String>,
    method: Method,
    kind: Option<String>,
) -> Operation {
    Operation {
        id: id.into(),
        family: family.into(),
        target: path.into(),
        desired: value,
        method,
        reset,
        sensor_type: kind,
    }
}
pub fn plan(h: &Hardware, c: &Config, t: Temperatures, nodes: &[Node]) -> Vec<Operation> {
    if !c.enabled {
        return vec![];
    }
    let mut ops = vec![];
    if c.charge_power_limit_enabled {
        let mask = c
            .charge_power_limit_protocols
            .iter()
            .fold(0, |m, p| m | p.bit());
        ops.push(write(
            crate::power::ID,
            "power",
            crate::power::DEVICE,
            format!("{} {} {}", c.revision, c.charge_power_limit_watts, mask),
            None,
            Method::Power,
            None,
        ));
    }
    if c.charge_pps_stability {
        ops.push(write(
            crate::pps::ID,
            "pps",
            crate::pps::ENABLED,
            "1".into(),
            Some("0".into()),
            Method::Pps,
            None,
        ));
    }
    for name in ["thermal-engine", ORMS] {
        ops.push(write(
            name,
            "services",
            name,
            "stopped".into(),
            Some("running".into()),
            Method::Service,
            None,
        ));
    }
    ops.push(write(
        "game_prepare",
        "frequency",
        GAME,
        "0".into(),
        Some("0".into()),
        Method::Write,
        None,
    ));
    for (name, value) in [("cpu_max_freq", 2147483647), ("cpu_min_freq", 0)] {
        let v = (0..8)
            .map(|i| format!("{i}:{value}"))
            .collect::<Vec<_>>()
            .join(" ");
        // The original resets these requests to unbounded/zero and does not replay an unreadable prior request.
        ops.push(write(
            name,
            "frequency",
            &format!("/proc/game_opt/{name}"),
            v.clone(),
            Some(v),
            Method::Write,
            None,
        ));
    }
    ops.push(write(
        "game_opt",
        "frequency",
        GAME,
        "1".into(),
        Some("0".into()),
        Method::Write,
        None,
    ));
    for (id, path) in [("bouncing", BOUNCE), ("omrg", OMRG), ("migt", MIGT)] {
        ops.push(write(
            id,
            "frequency",
            path,
            "0".into(),
            Some("1".into()),
            Method::Write,
            None,
        ));
    }
    for (path, kind) in discovery::cooling_nodes(h) {
        ops.push(write(
            &path,
            "cooling",
            &path,
            "0".into(),
            None,
            Method::Write,
            Some(kind),
        ));
    }
    if c.horae_stop {
        ops.push(write(
            "horae",
            "services",
            "horae",
            "stopped".into(),
            Some("running".into()),
            Method::Service,
            None,
        ));
    }
    for i in 0..h.capabilities().shell_slots.unwrap_or(0) {
        ops.push(write(
            &format!("shell_temp_{i}"),
            "shell",
            "/proc/shell-temp",
            format!("{i} {}", t.battery),
            Some(format!("{i} 0")),
            Method::Shell,
            None,
        ));
    }
    for n in nodes.iter().filter(|n| n.group == Group::Shell) {
        if let Some(p) = &n.emul_path {
            ops.push(write(
                p,
                "shell",
                p,
                t.battery.to_string(),
                Some("0".into()),
                Method::Emulation,
                Some(n.kind.clone()),
            ));
        }
    }
    for group in [Group::Cpu, Group::Gpu, Group::Ddr] {
        for n in nodes.iter().filter(|n| n.group == group) {
            let mc = match group {
                Group::Cpu => t.cpu,
                Group::Gpu => t.gpu,
                _ => t.ddr,
            };
            let raw = h
                .read(&n.path)
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);
            let family = match group {
                Group::Cpu => "cpu",
                Group::Gpu => "gpu",
                _ => "ddr",
            };
            ops.push(write(
                &n.path,
                family,
                &n.path,
                discovery::mask_raw(mc, raw).to_string(),
                None,
                Method::Bind {
                    source: mask_source(&n.path),
                },
                Some(n.kind.clone()),
            ));
        }
    }
    if c.charge_engineer_policy {
        ops.retain(|o| !temperature_input(o) && o.id != "horae");
        ops.push(write(
            crate::engineer::ID,
            "engineer",
            crate::engineer::TARGET,
            c.revision.to_string(),
            None,
            Method::Engineer,
            None,
        ));
    }
    ops
}
fn sensor_type(h: &Hardware, op: &Operation) -> Option<String> {
    let parent = op.target.rsplit_once('/')?.0;
    h.read(&format!("{parent}/type")).ok()
}
impl Controller {
    fn stamp(path: &std::path::Path) -> Option<[u64; 10]> {
        let m = fs::symlink_metadata(path).ok()?;
        Some([
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime() as u64,
            m.mtime_nsec() as u64,
            m.ctime() as u64,
            m.ctime_nsec() as u64,
            m.mode() as u64,
            m.uid() as u64,
            m.nlink(),
        ])
    }
    pub fn load(h: &Hardware) -> Result<Self> {
        let path = h.state().join("ownership.json");
        let boot = h
            .read("/proc/sys/kernel/random/boot_id")
            .unwrap_or_else(|_| "fixture".into());
        let persisted = if path.exists() {
            Some(storage::load::<Journal>(&path, 2_097_152)?)
        } else {
            None
        };
        let mut j = persisted.clone().unwrap_or_else(|| Journal {
            schema: 2,
            boot_id: boot.clone(),
            entries: BTreeMap::new(),
        });
        if !matches!(j.schema, 2 | 3) {
            return Err("ownership_schema_invalid".into());
        }
        for e in j.entries.values() {
            validate_operation(&e.op)?;
        }
        if j.boot_id != boot {
            j = Journal {
                schema: 2,
                boot_id: boot,
                entries: BTreeMap::new(),
            };
        }
        j.schema = 3;
        Ok(Self {
            journal: j,
            rounds: 0,
            stable: 0,
            power: crate::power::Session::default(),
            persisted: RefCell::new(persisted),
            persisted_stamp: RefCell::new(Self::stamp(&path)),
            statuses: BTreeMap::new(),
            verify_nodes: false,
            verification: BTreeMap::new(),
            repair_after: BTreeMap::new(),
        })
    }
    pub fn set_write_mode(&mut self, mode: WriteMode) {
        self.verify_nodes = mode == WriteMode::Event;
    }
    pub fn verification(&self, id: &str) -> Option<&Verification> {
        self.verification.get(id)
    }
    pub fn pending_repairs(&self) -> BTreeMap<String, u64> {
        self.statuses
            .iter()
            .filter_map(|(id, status)| {
                matches!(status.state.as_str(), "repair_wait" | "retrying")
                    .then(|| {
                        self.verification
                            .get(id)
                            .and_then(|info| info.retry_after_ms)
                            .map(|deadline| (id.clone(), deadline))
                    })
                    .flatten()
            })
            .collect()
    }
    pub fn verification_counters(&self) -> serde_json::Value {
        serde_json::json!({
            "verification_checks":self.verification.values().map(|v|v.verification_checks).sum::<u64>(),
            "repair_writes":self.verification.values().map(|v|v.repair_writes).sum::<u64>(),
            "skipped_writes":self.verification.values().map(|v|v.skipped_writes).sum::<u64>()
        })
    }
    fn save(&self, h: &Hardware) -> Result<()> {
        let path = h.state().join("ownership.json");
        if self.persisted.borrow().as_ref() == Some(&self.journal)
            && self.persisted_stamp.borrow().is_some()
            && *self.persisted_stamp.borrow() == Self::stamp(&path)
        {
            return Ok(());
        }
        if let Some(old) = self.persisted.borrow().as_ref() {
            if old.boot_id != self.journal.boot_id {
                storage::json(&h.state().join("ownership.previous-boot.json"), old)?;
            }
        }
        storage::json(&path, &self.journal)?;
        *self.persisted.borrow_mut() = Some(self.journal.clone());
        *self.persisted_stamp.borrow_mut() = Self::stamp(&path);
        Ok(())
    }
    pub fn reconcile(&mut self, h: &Hardware, ops: &[Operation]) -> Vec<BackendStatus> {
        self.reconcile_guarded(h, ops, || false)
    }
    pub fn reconcile_guarded<F: Fn() -> bool>(
        &mut self,
        h: &Hardware,
        ops: &[Operation],
        release_horae: F,
    ) -> Vec<BackendStatus> {
        let due = ops.iter().map(|op| op.id.clone()).collect();
        self.reconcile_selected(h, ops, &due, release_horae)
    }
    /// The complete desired set determines ownership; only `due` is maintained.
    pub fn reconcile_selected<F: Fn() -> bool>(
        &mut self,
        h: &Hardware,
        ops: &[Operation],
        due: &BTreeSet<String>,
        release_horae: F,
    ) -> Vec<BackendStatus> {
        let wanted = ops.iter().map(|o| o.id.as_str()).collect::<BTreeSet<_>>();
        let obsolete = self
            .journal
            .entries
            .keys()
            .filter(|k| !wanted.contains(k.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        self.statuses.retain(|id, _| {
            wanted.contains(id.as_str())
                || self.journal.entries.contains_key(id)
                || id == "ownership"
        });
        let journal_dirty = self.persisted.borrow().as_ref().is_some_and(|j| {
            j != &self.journal
                || *self.persisted_stamp.borrow() != Self::stamp(&h.state().join("ownership.json"))
        });
        if due.is_empty() && obsolete.is_empty() && !journal_dirty {
            return self.statuses.values().cloned().collect();
        }
        self.rounds += 1;
        let mut statuses = self.restore_ids(h, &obsolete);
        let mut changed = false;
        for op in ops.iter().filter(|op| due.contains(&op.id)) {
            if op.method == Method::Power {
                statuses.push(self.reconcile_power(h, op));
                continue;
            }
            if op.method == Method::Engineer {
                statuses.push(self.reconcile_engineer(h, op));
                continue;
            }
            // Do not reapply temperature masking while the old policy may still
            // be loaded in Battery. Recovery of this backend has priority.
            if !wanted.contains(crate::engineer::ID)
                && self.journal.entries.contains_key(crate::engineer::ID)
                && (temperature_input(op) || op.id == "horae")
            {
                statuses.push(BackendStatus {
                    id: op.id.clone(),
                    family: op.family.clone(),
                    target: op.target.clone(),
                    desired: op.desired.clone(),
                    state: "restore_pending".into(),
                    detail: Some("policy_restore_before_temperature_control".into()),
                });
                continue;
            }
            let previously_owned = self.journal.entries.contains_key(&op.id);
            let mut verification = self.verification.get(&op.id).cloned().unwrap_or_default();
            verification.write_performed = false;
            verification.verification = "not_checked".into();
            verification.observed_before = None;
            verification.observed_after = None;
            verification.retry_after_ms = None;
            verification.checked_ms = crate::runtime::now_ms();
            if release_horae() {
                if self.journal.entries.contains_key("horae") {
                    statuses.extend(self.yield_horae(h));
                }
                if op.id == "horae" {
                    continue;
                }
            }
            let result = (|| -> Result<&'static str> {
                validate_operation(op)?;
                if op.method == Method::Pps && !h.capabilities().pps_verified {
                    return Ok("unsupported");
                }
                if op.method == Method::Shell {
                    let Some(slots) = h.capabilities().shell_slots else {
                        return Ok("unverified");
                    };
                    if shell_index(op).is_none_or(|i| i >= slots) {
                        return Err("shell_slot_not_supported".into());
                    }
                }
                if op.method == Method::Service {
                    if h.service_state(&op.target)?.is_none() {
                        return Ok("unsupported");
                    }
                } else if op.method != Method::Pps && !h.present(&op.target)? {
                    return Ok("unavailable");
                }
                if op.sensor_type.is_some() && sensor_type(h, op) != op.sensor_type {
                    return Err("node_identity_changed_waiting_for_discovery".into());
                }
                if self
                    .journal
                    .entries
                    .get(&op.id)
                    .is_some_and(|e| e.op.sensor_type != op.sensor_type)
                {
                    let r = self.restore_ids(h, &[op.id.clone()]);
                    if self.journal.entries.contains_key(&op.id) {
                        return Err(format!("old_identity_restore_pending:{r:?}"));
                    }
                }
                if !self.journal.entries.contains_key(&op.id) {
                    let original = match op.method {
                        Method::Service => h.service_state(&op.target)?,
                        Method::Emulation | Method::Shell | Method::Bind { .. } => None,
                        Method::Write => h.read(&op.target).ok(),
                        Method::Pps => Some(crate::pps::original(h)?),
                        Method::Engineer | Method::Power => unreachable!(),
                    };
                    if op.method == Method::Service
                        && !matches!(original.as_deref(), Some("running" | "stopped"))
                    {
                        return Err("service_original_state_unavailable".into());
                    }
                    if op.method == Method::Write
                        && original.is_none()
                        && (op.reset.is_none() || (self.verify_nodes && can_verify(op)))
                    {
                        return Err("original_value_unavailable".into());
                    }
                    self.journal.entries.insert(
                        op.id.clone(),
                        Owned {
                            op: op.clone(),
                            original,
                            applied: false,
                            error: None,
                        },
                    );
                    if let Err(e) = self.save(h) {
                        self.journal.entries.remove(&op.id);
                        return Err(e);
                    }
                    changed = true;
                }
                if self
                    .journal
                    .entries
                    .get(&op.id)
                    .is_some_and(|e| e.op.desired != op.desired)
                {
                    let previous = self.journal.entries[&op.id].op.desired.clone();
                    self.journal.entries.get_mut(&op.id).unwrap().op.desired = op.desired.clone();
                    if let Err(error) = self.save(h) {
                        self.journal.entries.get_mut(&op.id).unwrap().op.desired = previous;
                        return Err(error);
                    }
                }
                // Re-establish a removed/replaced journal before any new device write.
                self.save(h)?;
                if op.method == Method::Pps {
                    crate::pps::prepare(h)?;
                }
                if let Method::Bind { source } = &op.method {
                    let needs_bind = !h.mounted(&op.target, source);
                    if needs_bind || h.read(source).ok().as_deref() != Some(op.desired.as_str()) {
                        h.mask_value(source, &op.target, &op.desired)?;
                        changed = true;
                    }
                    if needs_bind {
                        h.bind(source, &op.target)?;
                    }
                    if h.read(&op.target)? != op.desired {
                        return Err("bind_readback_mismatch".into());
                    }
                } else if op.method == Method::Service {
                    apply_service(h, &op.target, false)?;
                } else {
                    if self.verify_nodes && can_verify(op) {
                        verification.verification_checks += 1;
                        let current = h.read(&op.target).map_err(|error| {
                            verification.verification = "unreadable".into();
                            error
                        })?;
                        verification.observed_before = Some(current.clone());
                        if current.trim() == op.desired.trim() {
                            verification.observed_after = Some(current);
                            verification.verification = "matched".into();
                            verification.skipped_writes += 1;
                            verification.last_write_error = None;
                            return Ok("applied");
                        }
                        verification.verification = "mismatched".into();
                        if previously_owned {
                            if let Some(deadline) = self
                                .repair_after
                                .get(&op.id)
                                .filter(|deadline| crate::runtime::now_ms() < **deadline)
                            {
                                verification.retry_after_ms = Some(*deadline);
                                if let Some(error) = &verification.last_write_error {
                                    return Err(error.clone());
                                }
                                return Ok("repair_wait");
                            }
                        }
                    }
                    let write_result = h.write(&op.target, &op.desired);
                    if self.verify_nodes
                        && can_verify(op)
                        && (previously_owned || write_result.is_err())
                    {
                        self.repair_after.insert(
                            op.id.clone(),
                            crate::runtime::now_ms().saturating_add(self.interval_secs() * 1000),
                        );
                    }
                    verification.last_write_error = write_result.as_ref().err().cloned();
                    if write_result.is_err() {
                        verification.retry_after_ms = self.repair_after.get(&op.id).copied();
                    }
                    write_result?;
                    verification.write_performed = true;
                    if self.verify_nodes && can_verify(op) {
                        verification.repair_writes += 1;
                    }
                    if matches!(op.method, Method::Write | Method::Pps)
                        && !op.target.starts_with("/proc/game_opt/cpu_")
                    {
                        let readback = h.read(&op.target)?;
                        verification.observed_after = Some(readback.clone());
                        if readback.trim() != op.desired.trim() {
                            if self.verify_nodes && can_verify(op) {
                                self.repair_after.insert(
                                    op.id.clone(),
                                    crate::runtime::now_ms()
                                        .saturating_add(self.interval_secs() * 1000),
                                );
                                verification.retry_after_ms =
                                    self.repair_after.get(&op.id).copied();
                            }
                            return Err("node_readback_mismatch".into());
                        }
                        if can_verify(op) {
                            verification.verification = "repaired".into();
                        }
                    }
                }
                Ok("applied")
            })();
            let (state, detail) = match result {
                Ok(s) => (
                    s.to_owned(),
                    if s == "repair_wait" {
                        Some("node_repair_rate_limited".into())
                    } else if s == "unverified" {
                        Some("shell_driver_unverified".into())
                    } else if op.method == Method::Pps {
                        Some(
                            if s == "unsupported" {
                                "pps_firmware_not_supported"
                            } else {
                                "pps_status_assist_enabled_not_a_protocol_guarantee"
                            }
                            .into(),
                        )
                    } else if s == "applied" && op.method == Method::Shell {
                        Some("write_accepted_aggregate_readback".into())
                    } else if s == "applied" && op.target.starts_with("/proc/game_opt/cpu_") {
                        Some("write_accepted_readback_masked_by_switch".into())
                    } else if s == "applied" && op.method == Method::Emulation {
                        Some("write_accepted_no_readback".into())
                    } else {
                        None
                    },
                ),
                Err(e) => {
                    changed = true;
                    ("retrying".into(), Some(e))
                }
            };
            if let Some(e) = self.journal.entries.get_mut(&op.id) {
                e.applied = state == "applied";
                e.error = if state == "retrying" {
                    detail.clone()
                } else {
                    None
                };
            }
            statuses.push(BackendStatus {
                id: op.id.clone(),
                family: op.family.clone(),
                target: op.target.clone(),
                desired: op.desired.clone(),
                state,
                detail,
            });
            if can_verify(op) {
                self.verification.insert(op.id.clone(), verification);
            }
        }
        if let Err(e) = self.save(h) {
            statuses.push(BackendStatus {
                id: "ownership".into(),
                family: "storage".into(),
                target: "ownership.json".into(),
                desired: String::new(),
                state: "retrying".into(),
                detail: Some(e),
            });
        } else {
            self.statuses.remove("ownership");
        }
        if changed {
            self.stable = 0
        } else {
            self.stable = self.stable.saturating_add(1);
        }
        for status in statuses {
            self.statuses.insert(status.id.clone(), status);
        }
        self.statuses.values().cloned().collect()
    }
    fn reconcile_power(&mut self, h: &Hardware, op: &Operation) -> BackendStatus {
        let mut result = BackendStatus {
            id: op.id.clone(),
            family: op.family.clone(),
            target: op.target.clone(),
            desired: op.desired.clone(),
            state: "unsupported".into(),
            detail: None,
        };
        let action = (|| -> Result<String> {
            validate_operation(op)?;
            if !h.capabilities().pps_verified {
                return Ok("unsupported".into());
            }
            let previous = self.journal.entries.get(&op.id).cloned();
            if previous.is_none() {
                crate::power::check_available(h)?;
            }
            if previous.as_ref().is_none_or(|e| e.op != *op) {
                self.journal.entries.insert(
                    op.id.clone(),
                    Owned {
                        op: op.clone(),
                        original: Some("exclusive_private_vote".into()),
                        applied: false,
                        error: None,
                    },
                );
                if let Err(error) = self.save(h) {
                    if let Some(old) = previous {
                        self.journal.entries.insert(op.id.clone(), old);
                    } else {
                        self.journal.entries.remove(&op.id);
                    }
                    return Err(error);
                }
            }
            let state = self.power.apply(h, &op.desired)?;
            result.detail = Some(serde_json::to_string(&state).map_err(|e| e.to_string())?);
            Ok(
                if matches!(
                    state.state.as_str(),
                    "data_error" | "vote_rejected" | "conflict" | "retired"
                ) {
                    "retrying".into()
                } else {
                    state.state
                },
            )
        })();
        match action {
            Ok(state) => result.state = state,
            Err(error) => {
                result.state = if error == "power_firmware_not_supported" {
                    "unsupported"
                } else {
                    "retrying"
                }
                .into();
                result.detail = Some(error);
            }
        }
        if let Some(entry) = self.journal.entries.get_mut(&op.id) {
            entry.applied = result.state == "applied";
            entry.error = if result.state == "retrying" {
                result.detail.clone()
            } else {
                None
            };
        }
        result
    }
    fn save_engineer(&mut self, h: &Hardware, session: &crate::engineer::Session) -> Result<()> {
        let value = serde_json::to_string(session).map_err(|e| e.to_string())?;
        let entry = self
            .journal
            .entries
            .get_mut(crate::engineer::ID)
            .ok_or("policy_ownership_missing")?;
        let old = entry.original.replace(value);
        if let Err(e) = self.save(h) {
            self.journal
                .entries
                .get_mut(crate::engineer::ID)
                .unwrap()
                .original = old;
            return Err(e);
        }
        Ok(())
    }
    fn reconcile_engineer(&mut self, h: &Hardware, op: &Operation) -> BackendStatus {
        let mut result = BackendStatus {
            id: op.id.clone(),
            family: op.family.clone(),
            target: op.target.clone(),
            desired: op.desired.clone(),
            state: "unsupported".into(),
            detail: None,
        };
        let action = (|| -> Result<&'static str> {
            validate_operation(op)?;
            if self
                .journal
                .entries
                .get(&op.id)
                .is_some_and(|e| e.op.desired != op.desired)
            {
                let previous = self.journal.entries[&op.id].clone();
                let mut session = crate::engineer::Session::parse(previous.original.as_deref())?;
                if session.phase == crate::engineer::Phase::Applied && !session.failed {
                    // An unrelated config save does not alter the generated XML.
                    self.journal.entries.get_mut(&op.id).unwrap().op.desired = op.desired.clone();
                    if let Err(e) = self.save(h) {
                        self.journal.entries.insert(op.id.clone(), previous);
                        return Err(e);
                    }
                } else {
                    if session.phase == crate::engineer::Phase::ReloadRestore {
                        // Consume this explicit retry before issuing another
                        // restore reload; periodic reconciliation cannot repeat it.
                        session.phase = crate::engineer::Phase::Unmounting;
                        session.failed = true;
                        let entry = self.journal.entries.get_mut(&op.id).unwrap();
                        entry.op.desired = op.desired.clone();
                        entry.original =
                            Some(serde_json::to_string(&session).map_err(|e| e.to_string())?);
                        if let Err(e) = self.save(h) {
                            self.journal.entries.insert(op.id.clone(), previous);
                            return Err(e);
                        }
                    }
                    self.restore_ids(h, &[op.id.clone()]);
                    if self.journal.entries.contains_key(&op.id) {
                        return Err("policy_previous_restore_pending".into());
                    }
                }
            }
            if self
                .journal
                .entries
                .values()
                .any(|e| temperature_input(&e.op) || e.op.id == "horae")
            {
                return Err("policy_waiting_temperature_restore".into());
            }
            if !self.journal.entries.contains_key(&op.id) {
                let session = crate::engineer::prepare(h)?;
                self.journal.entries.insert(
                    op.id.clone(),
                    Owned {
                        op: op.clone(),
                        original: Some(serde_json::to_string(&session).map_err(|e| e.to_string())?),
                        applied: false,
                        error: None,
                    },
                );
                if let Err(e) = self.save(h) {
                    self.journal.entries.remove(&op.id);
                    return Err(e);
                }
            }
            self.save(h)?;
            let mut session =
                crate::engineer::Session::parse(self.journal.entries[&op.id].original.as_deref())?;
            if session.failed {
                if session.phase != crate::engineer::Phase::Restored {
                    crate::engineer::restore(h, &mut session, &mut |s| self.save_engineer(h, s))?;
                }
                return Ok("blocked");
            }
            if let Err(error) =
                crate::engineer::apply(h, &mut session, &mut |s| self.save_engineer(h, s))
            {
                session.failed = true;
                // On persistence failure stop here; keep the last durable phase
                // for recovery instead of issuing an unjournaled app restart.
                self.save_engineer(h, &session)?;
                let recovery =
                    crate::engineer::restore(h, &mut session, &mut |s| self.save_engineer(h, s));
                self.journal.entries.get_mut(&op.id).unwrap().error = Some(error.clone());
                self.save(h)?;
                recovery.map_err(|e| format!("{error};restore:{e}"))?;
                return Ok("blocked");
            }
            Ok("applied")
        })();
        match action {
            Ok(state) => {
                result.state = state.into();
                result.detail = self
                    .journal
                    .entries
                    .get(&op.id)
                    .and_then(|e| e.error.clone())
                    .or_else(|| Some("policy_config_applied_not_power_verified".into()));
            }
            Err(error) => {
                result.state = if self.journal.entries.contains_key(&op.id) {
                    "restore_pending"
                } else if error == "policy_waiting_temperature_restore" {
                    "retrying"
                } else {
                    "unsupported"
                }
                .into();
                result.detail = Some(error);
            }
        }
        if let Some(entry) = self.journal.entries.get_mut(&op.id) {
            entry.applied = result.state == "applied";
        }
        result
    }
    fn restore_ids(&mut self, h: &Hardware, ids: &[String]) -> Vec<BackendStatus> {
        let mut before = self.journal.clone();
        let mut entries = ids
            .iter()
            .filter_map(|id| self.journal.entries.get(id).cloned())
            .collect::<Vec<_>>();
        entries.sort_by_key(|e| match e.op.method {
            Method::Engineer | Method::Power => 0,
            Method::Bind { .. } => 1,
            Method::Shell | Method::Emulation => 2,
            Method::Write | Method::Pps => 3,
            Method::Service => 4,
        });
        let mut statuses = vec![];
        for e in entries {
            let op = &e.op;
            let r = (|| -> Result<()> {
                validate_operation(op)?;
                match &op.method {
                    Method::Engineer => {
                        let mut session = crate::engineer::Session::parse(e.original.as_deref())?;
                        crate::engineer::restore(h, &mut session, &mut |s| {
                            self.save_engineer(h, s)
                        })?;
                    }
                    Method::Pps => crate::pps::restore(h, e.original.as_deref())?,
                    Method::Power => self.power.restore(h)?,
                    Method::Bind { source } => {
                        h.unbind(&op.target, source)?;
                        let _ = fs::remove_file(h.path(source));
                    }
                    Method::Service => match e.original.as_deref() {
                        Some("running") => apply_service(h, &op.target, true)?,
                        Some("stopped") => (),
                        _ => return Err("service_restore_original_unknown".into()),
                    },
                    _ => {
                        if op.method == Method::Shell && h.capabilities().shell_slots.is_none() {
                            return Err("restore_shell_capability_unverified".into());
                        }
                        // The verified loaded driver rejects these indexes before touching memory.
                        // This also retires rc.4 records without issuing another invalid reset write.
                        if op.method == Method::Shell
                            && h.capabilities().shell_slots == Some(3)
                            && e.original.is_none()
                            && (3..6).any(|i| {
                                op.id == format!("shell_temp_{i}")
                                    && op.desired.split_whitespace().next()
                                        == Some(i.to_string().as_str())
                                    && op.reset.as_deref() == Some(format!("{i} 0").as_str())
                            })
                        {
                            return Ok(());
                        }
                        if op.method == Method::Shell
                            && shell_index(op)
                                .is_none_or(|i| i >= h.capabilities().shell_slots.unwrap_or(0))
                        {
                            return Err("restore_shell_slot_not_supported".into());
                        }
                        if !h.present(&op.target)? {
                            return Err("restore_target_missing".into());
                        }
                        if op.sensor_type.is_some() && sensor_type(h, op) != op.sensor_type {
                            return Ok(());
                        }
                        let value = if matches!(op.method, Method::Shell | Method::Emulation) {
                            op.reset.as_ref()
                        } else {
                            if op.target.starts_with("/proc/game_opt/cpu_") {
                                op.reset.as_ref()
                            } else {
                                e.original.as_ref().or(op.reset.as_ref())
                            }
                        };
                        let value = value.ok_or("restore_value_unknown")?;
                        h.write(&op.target, value)?;
                        if op.method == Method::Write
                            && !op.target.starts_with("/proc/game_opt/cpu_")
                            && h.read(&op.target)?.trim() != value.trim()
                        {
                            return Err("restore_readback_mismatch".into());
                        }
                    }
                }
                Ok(())
            })();
            let detail = r.err();
            let restored = detail.is_none();
            if restored {
                self.journal.entries.remove(&op.id);
            } else if let Some(ent) = self.journal.entries.get_mut(&op.id) {
                ent.error = detail.clone();
            }
            statuses.push(BackendStatus {
                id: op.id.clone(),
                family: op.family.clone(),
                target: op.target.clone(),
                desired: String::new(),
                state: if restored {
                    "restored"
                } else {
                    "restore_pending"
                }
                .into(),
                detail,
            });
        }
        if !ids.is_empty() {
            if let Err(error) = self.save(h) {
                // Preserve the latest durable reload checkpoint. Reverting to
                // the pre-restore phase could issue the same restart twice.
                if let Some(entry) = self
                    .persisted
                    .borrow()
                    .as_ref()
                    .and_then(|j| j.entries.get(crate::engineer::ID))
                    .cloned()
                {
                    before.entries.insert(crate::engineer::ID.into(), entry);
                }
                self.journal = before;
                for status in &mut statuses {
                    status.state = "restore_pending".into();
                    status.detail = Some(format!("restore_journal_save:{error}"));
                }
            }
        }
        statuses
    }
    pub fn restore(&mut self, h: &Hardware) -> Result<Vec<BackendStatus>> {
        let ids = self.journal.entries.keys().cloned().collect::<Vec<_>>();
        let reports = self.restore_ids(h, &ids);
        self.save(h)?;
        Ok(reports)
    }
    pub fn yield_horae(&mut self, h: &Hardware) -> Vec<BackendStatus> {
        self.restore_ids(h, &["horae".into()])
    }
    pub fn interval_secs(&self) -> u64 {
        if self.stable >= 8 {
            3
        } else {
            2
        }
    }
}
fn apply_service(h: &Hardware, name: &str, running: bool) -> Result<()> {
    h.service(name, running)
}
fn shell_index(op: &Operation) -> Option<usize> {
    let index = op.id.strip_prefix("shell_temp_")?.parse::<usize>().ok()?;
    let written = op
        .desired
        .split_whitespace()
        .next()?
        .parse::<usize>()
        .ok()?;
    (index == written).then_some(index)
}
fn validate_operation(op: &Operation) -> Result<()> {
    let t = &op.target;
    let thermal = ["/sys/class/thermal/", "/sys/devices/virtual/thermal/"]
        .iter()
        .any(|p| {
            t.strip_prefix(p).is_some_and(|x| {
                x.split_once('/').is_some_and(|(node, leaf)| {
                    (discovery::numeric(node, "thermal_zone")
                        && matches!(leaf, "temp" | "emul_temp"))
                        || (discovery::numeric(node, "cooling_device") && leaf == "cur_state")
                })
            })
        });
    let ok = match &op.method {
        Method::Power => {
            op.id == crate::power::ID
                && op.family == "power"
                && t == crate::power::DEVICE
                && crate::power::settings(&op.desired).is_ok()
                && op.reset.is_none()
                && op.sensor_type.is_none()
        }
        Method::Engineer => {
            op.id == crate::engineer::ID
                && op.family == "engineer"
                && t == crate::engineer::TARGET
                && op.desired.parse::<u64>().is_ok()
                && op.reset.is_none()
                && op.sensor_type.is_none()
        }
        Method::Pps => {
            op.id == crate::pps::ID
                && op.family == "pps"
                && t == crate::pps::ENABLED
                && op.desired == "1"
                && op.reset.as_deref() == Some("0")
                && op.sensor_type.is_none()
        }
        Method::Service => matches!(t.as_str(), "horae" | "thermal-engine" | ORMS),
        Method::Bind { source } => thermal && t.ends_with("/temp") && *source == mask_source(t),
        Method::Emulation => thermal && t.ends_with("/emul_temp"),
        Method::Shell => t == "/proc/shell-temp",
        Method::Write => {
            thermal
                || matches!(
                    t.as_str(),
                    GAME | BOUNCE
                        | OMRG
                        | MIGT
                        | "/proc/game_opt/cpu_max_freq"
                        | "/proc/game_opt/cpu_min_freq"
                )
        }
    };
    if ok {
        Ok(())
    } else {
        Err("invalid_owned_operation".into())
    }
}
