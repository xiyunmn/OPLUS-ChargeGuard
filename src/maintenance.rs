//! Schedule subsets of a complete desired plan, retaining per-backend retry cadence.
use crate::{
    config::WriteMode,
    control::{can_verify, BackendStatus, Method, Operation},
    maintenance_events::Changes,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct Schedule {
    operations: Vec<Operation>,
    mode: Option<WriteMode>,
    pending: BTreeMap<String, (&'static str, bool)>,
    checked: BTreeMap<String, u64>,
    triggers: BTreeMap<String, &'static str>,
    event_ids: BTreeSet<String>,
    dormant: BTreeSet<String>,
    next_tick: u64,
    settle: BTreeMap<String, u64>,
    repair_deadlines: BTreeMap<String, u64>,
    receipts: BTreeMap<String, Vec<serde_json::Value>>,
}
impl Schedule {
    pub fn set_plan(&mut self, mode: WriteMode, ops: &[Operation]) {
        let mode_changed = self.mode.is_some_and(|m| m != mode);
        for op in ops {
            if mode_changed || !self.operations.iter().any(|old| old == op) {
                self.pending.insert(
                    op.id.clone(),
                    (
                        if mode_changed {
                            "mode_changed"
                        } else {
                            "plan_changed"
                        },
                        true,
                    ),
                );
            }
        }
        let wanted = ops.iter().map(|o| o.id.as_str()).collect::<BTreeSet<_>>();
        self.pending.retain(|id, _| wanted.contains(id.as_str()));
        self.checked.retain(|id, _| wanted.contains(id.as_str()));
        self.triggers.retain(|id, _| wanted.contains(id.as_str()));
        self.event_ids.retain(|id| wanted.contains(id.as_str()));
        self.dormant.retain(|id| wanted.contains(id.as_str()));
        self.settle.retain(|id, _| wanted.contains(id.as_str()));
        self.repair_deadlines
            .retain(|id, _| wanted.contains(id.as_str()));
        self.receipts.retain(|id, _| wanted.contains(id.as_str()));
        self.operations = ops.to_vec();
        self.mode = Some(mode);
        if mode_changed {
            self.next_tick = 0;
        }
    }
    pub fn notify(&mut self, changes: Changes) {
        for (id, events) in changes.receipts {
            let pending = self.receipts.entry(id).or_default();
            pending.extend(
                events
                    .into_iter()
                    .take(8usize.saturating_sub(pending.len())),
            );
        }
        for op in &self.operations {
            let cause = changes.ids.get(&op.id).copied().or_else(|| {
                (changes.bindings && matches!(op.method, Method::Bind { .. }))
                    .then_some("mount_or_listener_event")
            });
            if let Some(cause) = cause {
                let urgent = self.mode == Some(WriteMode::Event)
                    && matches!(
                        cause,
                        "node_file_event"
                            | "node_subscribed"
                            | "thermal/cdev_update"
                            | "thermal_netlink"
                            | "cooling_topology"
                            | "cooling_listener_lost"
                    )
                    && can_verify(op);
                if urgent && matches!(cause, "thermal/cdev_update" | "thermal_netlink") {
                    // A request notification can precede the driver's actual state update.
                    // One bounded follow-up per burst; this is not a periodic audit.
                    self.settle
                        .entry(op.id.clone())
                        .or_insert(crate::runtime::now_ms() + 50);
                }
                // Readable nodes can be checked immediately without replaying a write.
                // Upgrade a queued periodic check; retaining its old urgency would hide
                // a drift event until the normal interval expires.
                self.pending
                    .entry(op.id.clone())
                    .and_modify(|queued| {
                        if urgent && !queued.1 {
                            *queued = (cause, true);
                        }
                    })
                    .or_insert((cause, urgent));
            }
        }
    }
    /// `now` is supplied by the worker's boottime clock, including time suspended.
    pub fn due(
        &mut self,
        now: u64,
        interval_ms: u64,
        mut event_ids: BTreeSet<String>,
        statuses: &[BackendStatus],
    ) -> BTreeSet<String> {
        for (id, deadline) in &self.repair_deadlines {
            if now >= *deadline {
                self.pending
                    .insert(id.clone(), ("repair_window_elapsed", true));
            }
        }
        if self.mode != Some(WriteMode::Event) {
            event_ids.clear();
            self.settle.clear();
        }
        for (id, deadline) in &self.settle {
            if now >= *deadline {
                self.pending
                    .entry(id.clone())
                    .or_insert(("cooling_event_settle", true));
            }
        }
        self.settle.retain(|_, deadline| now < *deadline);
        // Changing a subscription requires a fresh observation, not release of ownership.
        for id in self.event_ids.symmetric_difference(&event_ids) {
            self.pending
                .insert(id.clone(), ("subscription_changed", true));
        }
        self.event_ids = event_ids;
        self.dormant = statuses
            .iter()
            .filter(|s| {
                matches!(
                    s.id.as_str(),
                    "omrg" | "migt" | crate::control::ORMS | crate::pps::ID
                ) && matches!(s.state.as_str(), "unavailable" | "unsupported")
            })
            .map(|s| s.id.clone())
            .collect();
        let periodic = self
            .operations
            .iter()
            .filter(|op| {
                !self.dormant.contains(&op.id)
                    && (!self.event_ids.contains(&op.id)
                        || statuses.iter().any(|s| {
                            s.id == op.id
                                && matches!(s.state.as_str(), "retrying" | "restore_pending")
                        }))
            })
            .collect::<Vec<_>>();
        if periodic.is_empty() {
            self.next_tick = u64::MAX;
        } else if self.next_tick == u64::MAX {
            self.next_tick = now;
        }
        if now >= self.next_tick {
            for op in periodic {
                let retry = statuses.iter().any(|s| {
                    s.id == op.id && matches!(s.state.as_str(), "retrying" | "restore_pending")
                });
                if !self.event_ids.contains(&op.id) || retry {
                    self.pending
                        .entry(op.id.clone())
                        .or_insert((if retry { "retry" } else { "periodic" }, false));
                }
            }
            self.next_tick = now.saturating_add(interval_ms);
        }
        self.pending
            .iter()
            .filter_map(|(id, (_, urgent))| {
                (*urgent
                    || self
                        .checked
                        .get(id)
                        .is_none_or(|t| now >= t.saturating_add(interval_ms)))
                .then(|| id.clone())
            })
            .collect()
    }
    pub fn completed(&mut self, ids: &BTreeSet<String>, now: u64) {
        for id in ids {
            if let Some((cause, _)) = self.pending.remove(id) {
                self.triggers.insert(id.clone(), cause);
            }
            self.checked.insert(id.clone(), now);
            self.receipts.remove(id);
        }
    }
    pub fn set_repair_deadlines(&mut self, mut deadlines: BTreeMap<String, u64>) {
        deadlines.retain(|id, _| self.operations.iter().any(|op| &op.id == id));
        self.repair_deadlines = deadlines;
    }
    pub fn rediscover(&mut self, include_services: bool) {
        for id in &self.dormant {
            if include_services || id != crate::control::ORMS {
                self.pending.insert(id.clone(), ("rediscovery", true));
            }
        }
    }
    pub fn wait_ms(&self, now: u64, interval_ms: u64) -> u64 {
        let pending = self
            .pending
            .iter()
            .map(|(id, (_, urgent))| {
                if *urgent {
                    now
                } else {
                    self.checked
                        .get(id)
                        .map_or(now, |t| t.saturating_add(interval_ms))
                }
            })
            .min()
            .unwrap_or(u64::MAX);
        self.next_tick
            .min(pending)
            .min(self.settle.values().copied().min().unwrap_or(u64::MAX))
            .min(
                self.repair_deadlines
                    .values()
                    .copied()
                    .min()
                    .unwrap_or(u64::MAX),
            )
            .saturating_sub(now)
    }
    pub fn last_checked(&self, id: &str) -> Option<u64> {
        self.checked.get(id).copied()
    }
    pub fn pending_cause(&self, id: &str) -> Option<&'static str> {
        self.pending.get(id).map(|p| p.0)
    }
    pub fn receipts(&self, id: &str) -> Option<&Vec<serde_json::Value>> {
        self.receipts.get(id)
    }
    pub fn last_trigger(&self, id: &str) -> Option<&'static str> {
        self.triggers.get(id).copied()
    }
    pub fn is_event(&self, id: &str) -> bool {
        self.event_ids.contains(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(id: &str, family: &str, method: Method) -> Operation {
        Operation {
            id: id.into(),
            family: family.into(),
            target: if id == "bouncing" {
                crate::control::BOUNCE.into()
            } else {
                id.into()
            },
            desired: "0".into(),
            method,
            reset: None,
            sensor_type: None,
        }
    }
    fn ids(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).into()).collect()
    }
    fn notify(schedule: &mut Schedule, names: &[&str], cause: &'static str) {
        schedule.notify(Changes {
            ids: names.iter().map(|name| ((*name).into(), cause)).collect(),
            ..Default::default()
        });
    }
    fn initialized(mode: WriteMode, ops: &[Operation]) -> Schedule {
        let mut schedule = Schedule::default();
        schedule.set_plan(mode, ops);
        let due = schedule.due(1000, 3000, BTreeSet::new(), &[]);
        schedule.completed(&due, 1000);
        schedule
    }

    #[test]
    fn readable_node_event_upgrades_a_rate_limited_periodic_check() {
        let ops = [op("bouncing", "frequency", Method::Write)];
        let mut schedule = initialized(WriteMode::Event, &ops);
        notify(&mut schedule, &["bouncing"], "node_file_event");
        let due = schedule.due(1100, 3000, BTreeSet::new(), &[]);
        assert_eq!(due, ids(&["bouncing"]));
        schedule.completed(&due, 1100);
        // The periodic tick is due, but its per-node interval has not elapsed.
        assert!(schedule.due(4000, 3000, BTreeSet::new(), &[]).is_empty());
        assert_eq!(schedule.wait_ms(4000, 3000), 100);
        notify(&mut schedule, &["bouncing"], "node_file_event");
        assert_eq!(schedule.wait_ms(4050, 3000), 0);
        let due = schedule.due(4050, 3000, BTreeSet::new(), &[]);
        assert_eq!(due, ids(&["bouncing"]));
        schedule.completed(&due, 4050);
        assert_eq!(schedule.last_trigger("bouncing"), Some("node_file_event"));
        assert!(schedule.due(4051, 3000, BTreeSet::new(), &[]).is_empty());
    }

    #[test]
    fn only_readable_node_events_are_urgent_and_duplicates_coalesce() {
        let ops = [
            op("bouncing", "frequency", Method::Write),
            op(
                "/sys/class/thermal/cooling_device0/cur_state",
                "cooling",
                Method::Write,
            ),
            op("game_opt", "frequency", Method::Write),
            op("shell_temp_0", "shell", Method::Shell),
            op("emul_temp", "shell", Method::Emulation),
            op("horae", "services", Method::Service),
        ];
        let mut schedule = initialized(WriteMode::Event, &ops);
        let names = ops.iter().map(|op| op.id.as_str()).collect::<Vec<_>>();
        for _ in 0..20 {
            notify(&mut schedule, &names, "node_file_event");
        }
        let readable = ids(&["bouncing", "/sys/class/thermal/cooling_device0/cur_state"]);
        let due = schedule.due(1100, 3000, BTreeSet::new(), &[]);
        assert_eq!(due, readable);
        schedule.completed(&due, 1100);
        assert!(schedule.due(1101, 3000, BTreeSet::new(), &[]).is_empty());
        let due = schedule.due(4000, 3000, BTreeSet::new(), &[]);
        assert_eq!(
            due,
            ids(&["game_opt", "shell_temp_0", "emul_temp", "horae"])
        );
    }

    #[test]
    fn loop_mode_keeps_node_events_on_periodic_cadence() {
        let ops = [op("bouncing", "frequency", Method::Write)];
        let mut schedule = initialized(WriteMode::Loop, &ops);
        notify(&mut schedule, &["bouncing"], "node_file_event");
        // Even an accidentally supplied subscription cannot disable loop maintenance.
        assert!(schedule.due(1100, 3000, ids(&["bouncing"]), &[]).is_empty());
        assert_eq!(
            schedule.due(4000, 3000, ids(&["bouncing"]), &[]),
            ids(&["bouncing"])
        );
    }

    #[test]
    fn bouncing_is_pure_event_until_its_listener_is_lost() {
        let ops = [op("bouncing", "frequency", Method::Write)];
        let mut schedule = initialized(WriteMode::Event, &ops);
        let subscribed = ids(&["bouncing"]);
        let due = schedule.due(1001, 3000, subscribed.clone(), &[]);
        schedule.completed(&due, 1001);
        assert!(schedule.due(4001, 3000, subscribed, &[]).is_empty());
        let due = schedule.due(4100, 3000, BTreeSet::new(), &[]);
        assert_eq!(due, ids(&["bouncing"]));
        schedule.completed(&due, 4100);
        assert_eq!(
            schedule.due(7100, 3000, BTreeSet::new(), &[]),
            ids(&["bouncing"])
        );
    }

    #[test]
    fn cooling_pure_event_has_no_periodic_reads_and_loss_restores_local_audit() {
        let ops = [
            op(
                "/sys/class/thermal/cooling_device0/cur_state",
                "cooling",
                Method::Write,
            ),
            op("bouncing", "frequency", Method::Write),
        ];
        let mut schedule = initialized(WriteMode::Event, &ops);
        let cooling = ids(&[ops[0].id.as_str()]);
        let due = schedule.due(1001, 3000, cooling.clone(), &[]);
        schedule.completed(&due, 1001);
        assert_eq!(
            schedule.due(4001, 3000, cooling.clone(), &[]),
            ids(&["bouncing"])
        );
        schedule.completed(&ids(&["bouncing"]), 4001);
        notify(&mut schedule, &[ops[0].id.as_str()], "node_file_event");
        assert_eq!(schedule.due(4100, 3000, cooling.clone(), &[]), cooling);
        schedule.completed(&cooling, 4100);
        assert_eq!(schedule.due(4200, 3000, BTreeSet::new(), &[]), cooling);
        schedule.completed(&cooling, 4200);
        assert_eq!(schedule.due(7200, 3000, BTreeSet::new(), &[]).len(), 2);
    }
    #[test]
    fn kernel_notification_gets_a_single_bounded_settle_check() {
        let ops = [op(
            "/sys/class/thermal/cooling_device0/cur_state",
            "cooling",
            Method::Write,
        )];
        let mut schedule = Schedule::default();
        schedule.set_plan(WriteMode::Event, &ops);
        let now = crate::runtime::now_ms();
        let cooling = ids(&[ops[0].id.as_str()]);
        let due = schedule.due(now, 3000, cooling.clone(), &[]);
        schedule.completed(&due, now);
        notify(&mut schedule, &[ops[0].id.as_str()], "thermal/cdev_update");
        let due = schedule.due(now, 3000, cooling.clone(), &[]);
        assert_eq!(due, cooling);
        schedule.completed(&due, now);
        let due = schedule.due(now + 100, 3000, cooling.clone(), &[]);
        assert_eq!(due, cooling);
        assert_eq!(
            schedule.pending_cause(&ops[0].id),
            Some("cooling_event_settle")
        );
        schedule.completed(&due, now + 100);
        assert!(schedule.due(now + 9000, 3000, cooling, &[]).is_empty());
    }
    #[test]
    fn boottime_advance_uses_one_check_without_replaying_missed_ticks() {
        let ops = [op("bouncing", "frequency", Method::Write)];
        let mut schedule = initialized(WriteMode::Event, &ops);
        let due = schedule.due(120_000, 3000, BTreeSet::new(), &[]);
        assert_eq!(due, ids(&["bouncing"]));
        schedule.completed(&due, 120_000);
        assert!(schedule.due(120_001, 3000, BTreeSet::new(), &[]).is_empty());
        assert_eq!(schedule.wait_ms(120_001, 3000), 2999);
    }

    #[test]
    fn drift_wait_uses_original_deadline_without_another_external_event() {
        let ops = [op("bouncing", "frequency", Method::Write)];
        let mut schedule = initialized(WriteMode::Event, &ops);
        let event_ids = ids(&["bouncing"]);
        let due = schedule.due(1001, 3000, event_ids.clone(), &[]);
        schedule.completed(&due, 1001);
        // A drift arrived near the end of the write cooldown. Checking it must not
        // postpone repair for another full interval from the time of this check.
        notify(&mut schedule, &["bouncing"], "node_file_event");
        let due = schedule.due(3800, 3000, event_ids.clone(), &[]);
        schedule.completed(&due, 3800);
        schedule.set_repair_deadlines(BTreeMap::from([("bouncing".into(), 4000)]));
        assert_eq!(schedule.wait_ms(3800, 3000), 200);
        assert!(schedule.due(3999, 3000, event_ids.clone(), &[]).is_empty());
        let due = schedule.due(4000, 3000, event_ids.clone(), &[]);
        assert_eq!(due, event_ids);
        assert_eq!(
            schedule.pending_cause("bouncing"),
            Some("repair_window_elapsed")
        );
        schedule.completed(&due, 4000);
        schedule.set_repair_deadlines(BTreeMap::new());
        assert_eq!(schedule.wait_ms(4001, 3000), u64::MAX - 4001);
        assert!(schedule.due(9000, 3000, event_ids, &[]).is_empty());
    }

    #[test]
    fn plan_removal_cancels_a_pending_repair_deadline() {
        let ops = [op("bouncing", "frequency", Method::Write)];
        let mut schedule = initialized(WriteMode::Event, &ops);
        schedule.set_repair_deadlines(BTreeMap::from([("bouncing".into(), 4000)]));
        schedule.set_plan(WriteMode::Event, &[]);
        assert!(schedule.due(5000, 3000, BTreeSet::new(), &[]).is_empty());
        assert!(schedule.repair_deadlines.is_empty());
    }
}
