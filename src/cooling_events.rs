//! Kernel notification transport for cooling. Never reads/writes a control node.
use crate::{control::Operation, hardware::Hardware, maintenance_events::Changes, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::Read,
    os::fd::AsRawFd,
    process::{Child, ChildStdout, Command, Stdio},
};

#[derive(Default)]
struct Protocol {
    sequence: u64,
    started: bool,
    layout: Option<(usize, usize, usize)>,
    ready: bool,
    deadline: u64,
    received: BTreeMap<String, u64>,
    matched: BTreeMap<String, u64>,
    online_cpus: Value,
}
pub struct CoolingEvents {
    child: Child,
    output: ChildStdout,
    pending: Vec<u8>,
    protocol: Protocol,
}
fn field(format: &str, name: &str) -> Option<(usize, usize)> {
    let line = format.lines().find(|line| {
        line.split(';')
            .next()
            .is_some_and(|f| f.split_whitespace().last() == Some(name))
    })?;
    let number = |key| {
        line.split(';')
            .find_map(|p| p.trim().strip_prefix(key)?.trim().parse::<usize>().ok())
    };
    Some((number("offset:")?, number("size:")?))
}
fn layout(format: &str) -> Option<(usize, usize, usize)> {
    let (kind, size) = field(format, "type")?;
    let (target, target_size) = field(format, "target")?;
    if size != 4
        || !matches!(target_size, 4 | 8)
        || kind > 4096
        || target > 4096
        || !format
            .lines()
            .any(|l| l.contains("field:__data_loc char[] type;"))
    {
        return None;
    }
    Some((kind, target, target_size))
}
fn decode(event: &Value, layout: (usize, usize, usize)) -> Result<(String, u64)> {
    let hex = event["raw_hex"].as_str().ok_or("cooling_missing_raw")?;
    if hex.len() > 16384 || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("cooling_invalid_raw".into());
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let (offset, target, size) = layout;
    let loc = u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or("cooling_short_type")?
            .try_into()
            .unwrap(),
    );
    let start = (loc & 65535) as usize;
    let length = (loc >> 16) as usize;
    let text = bytes
        .get(start..start + length)
        .filter(|b| b.last() == Some(&0))
        .ok_or("cooling_invalid_type")?;
    let kind = std::str::from_utf8(&text[..text.len() - 1]).map_err(|_| "cooling_type_encoding")?;
    if kind.is_empty() || kind.len() > 256 || kind.contains('\0') {
        return Err("cooling_type_length".into());
    }
    let mut number = [0u8; 8];
    number[..size].copy_from_slice(
        bytes
            .get(target..target + size)
            .ok_or("cooling_short_target")?,
    );
    Ok((kind.into(), u64::from_le_bytes(number)))
}
impl Protocol {
    fn accept(
        &mut self,
        event: &Value,
        ops: &[Operation],
        changes: &mut Changes,
        now: u64,
    ) -> Result<()> {
        let sequence = event["sequence"]
            .as_u64()
            .ok_or("cooling_missing_sequence")?;
        if sequence != self.sequence + 1 {
            return Err("cooling_notification_sequence_gap".into());
        }
        self.sequence = sequence;
        let kind = event["event"].as_str().ok_or("cooling_missing_event")?;
        if !self.started {
            if kind != "cooling_listener_start"
                || event["control_reads"] != false
                || event["writes_controls"] != false
                || event["parent_pipe"] != true
            {
                return Err("cooling_invalid_handshake".into());
            }
            self.started = true;
            return Ok(());
        }
        let mut cdev_type = None;
        let mut cdev_id = None;
        let source = match kind {
            "trace_format" if event["source"] == "thermal/cdev_update" => {
                self.layout = event["format"].as_str().and_then(layout);
                if self.layout.is_none() {
                    return Err("cooling_trace_layout_unsupported".into());
                }
                return Ok(());
            }
            "cooling_health" => {
                let cpus = event["online_cpus"]
                    .as_array()
                    .filter(|v| !v.is_empty())
                    .ok_or("cooling_no_cpu_coverage")?;
                if !cpus.iter().all(|v| v.as_u64().is_some_and(|n| n < 64)) {
                    return Err("cooling_invalid_cpu_coverage".into());
                }
                if event["lost_records"].as_u64() != Some(0) {
                    return Err("cooling_notification_loss".into());
                }
                let ready = event["netlink_ready"] == true
                    && event["uevent_ready"] == true
                    && event["perf_complete"] == true
                    && self.layout.is_some();
                changes.health |= ready != self.ready;
                self.ready = ready;
                self.online_cpus = event["online_cpus"].clone();
                self.deadline = now.saturating_add(20_000);
                return Ok(());
            }
            "kernel_event" if event["source"] == "thermal/cdev_update" => {
                let (name, _) = decode(event, self.layout.ok_or("cooling_missing_trace_layout")?)?;
                cdev_type = Some(name);
                "thermal/cdev_update"
            }
            "thermal_netlink" => {
                if let Some(id) = event["cdev_id"].as_i64().filter(|id| *id >= 0) {
                    cdev_id = Some(id);
                } else {
                    return Ok(());
                }
                "thermal_netlink"
            }
            "topology_event" => {
                // A CPU can come online before the next perf subscription is installed.
                self.ready = false;
                changes.health = true;
                "cooling_topology"
            }
            "source_unavailable"
            | "perf_unavailable"
            | "notification_loss"
            | "cooling_listener_end" => {
                return Err(format!(
                    "cooling_transport:{kind}:{}",
                    event["source"].as_str().unwrap_or("pipe")
                ))
            }
            "source_ready" | "perf_ready" | "listeners_rebuild" => return Ok(()),
            _ => return Err("cooling_unknown_protocol_event".into()),
        };
        *self.received.entry(source.into()).or_default() += 1;
        for op in ops.iter().filter(|op| op.family == "cooling") {
            let selected = cdev_type
                .as_ref()
                .is_some_and(|kind| op.sensor_type.as_ref() == Some(kind))
                || cdev_id.is_some_and(|id| {
                    op.target
                        .ends_with(&format!("/cooling_device{id}/cur_state"))
                })
                || source == "cooling_topology";
            if !selected {
                continue;
            }
            *self.matched.entry(source.into()).or_default() += 1;
            changes.ids.insert(op.id.clone(), source);
            let mut receipt = event.clone();
            receipt["source"] = json!(source);
            receipt["received_ms"] = json!(now);
            receipt["writer"] = json!("unknown");
            if let (Some(at), Some(handled)) = (
                event["event_monotonic_ns"].as_u64(),
                event["monotonic_ns"].as_u64(),
            ) {
                receipt["kernel_to_transport_ms"] = json!(handled.saturating_sub(at) / 1_000_000);
            }
            // Keep transport data separate from actual cur_state readback.
            if source == "thermal/cdev_update" {
                let (kind, requested) = decode(event, self.layout.unwrap())?;
                receipt["cdev_type"] = json!(kind);
                receipt["requested_target"] = json!(requested);
            }
            let receipts = changes.receipts.entry(op.id.clone()).or_default();
            if receipts.len() < 8 {
                receipts.push(receipt);
            }
        }
        Ok(())
    }
}
impl CoolingEvents {
    pub fn start(h: &Hardware) -> Result<Self> {
        let mut child = Command::new(h.path(&format!("{}/bin/cg-cooling-events", crate::MODULE)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cooling_spawn:{e}"))?;
        let output = child.stdout.take().ok_or("cooling_missing_pipe")?;
        let fd = output.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cooling_nonblocking_pipe".into());
        }
        Ok(Self {
            child,
            output,
            pending: vec![],
            protocol: Protocol {
                deadline: crate::runtime::now_ms() + 8000,
                ..Default::default()
            },
        })
    }
    pub fn fd(&self) -> i32 {
        self.output.as_raw_fd()
    }
    pub fn ready(&self) -> bool {
        self.protocol.ready && crate::runtime::now_ms() < self.protocol.deadline
    }
    pub fn wait_ms(&self) -> u64 {
        self.protocol
            .deadline
            .saturating_sub(crate::runtime::now_ms())
    }
    pub fn health(&self) -> Value {
        json!({"ready":self.ready(),"received_by_source":self.protocol.received,"matched_by_source":self.protocol.matched,"online_cpus":self.protocol.online_cpus,"periodic_control_reads":false})
    }
    pub fn drain(&mut self, ops: &[Operation], changes: &mut Changes) -> Result<()> {
        let mut buf = [0u8; 8192];
        for _ in 0..16 {
            match self.output.read(&mut buf) {
                Ok(0) => return Err("cooling_listener_eof".into()),
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("cooling_pipe:{e}")),
            }
        }
        if self.pending.len() > 262144 {
            return Err("cooling_pipe_backlog".into());
        }
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line = self.pending.drain(..=end).collect::<Vec<_>>();
            let event: Value = serde_json::from_slice(&line).map_err(|_| "cooling_invalid_json")?;
            self.protocol
                .accept(&event, ops, changes, crate::runtime::now_ms())?;
        }
        if self.wait_ms() == 0 {
            return Err("cooling_listener_health_timeout".into());
        }
        Ok(())
    }
}
impl Drop for CoolingEvents {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
