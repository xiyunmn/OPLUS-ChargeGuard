//! Notifications for owned masks/mounts and init service state. No device writes.
use crate::{
    control::{BackendStatus, Method, Operation, BOUNCE, ORMS},
    hardware::{Hardware, MASKS},
    Result,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::ffi::OsStrExt,
    process::{Child, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

const FILES: u64 = 1;
const MOUNTS: u64 = 2;
const SERVICES: u64 = 3;
const COOLING: u64 = 4;
pub const SERVICE_NAMES: [&str; 3] = ["thermal-engine", ORMS, "horae"];
const MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_ATTRIB
    | libc::IN_CREATE
    | libc::IN_MOVED_TO
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF;

#[derive(Default)]
pub struct Changes {
    pub ids: BTreeMap<String, &'static str>,
    pub receipts: BTreeMap<String, Vec<serde_json::Value>>,
    pub bindings: bool,
    pub health: bool,
}
#[derive(Clone)]
enum Watch {
    Masks,
    Mask(String),
    Node(String),
    Service(String),
    ServiceDirectory,
    FixtureMounts,
}

pub struct ControlEvents {
    cooling: Option<crate::cooling_events::CoolingEvents>,
    cooling_error: Option<String>,
    cooling_retry: Instant,
    cooling_reconnections: u64,
    node_events: BTreeMap<String, u64>,
    epoll: OwnedFd,
    files: Option<OwnedFd>,
    mounts: Option<File>,
    child: Option<Child>,
    service_deadline: Option<Instant>,
    output: Option<ChildStdout>,
    pending: Vec<u8>,
    watches: BTreeMap<i32, Watch>,
    mask_watches: BTreeSet<String>,
    node_watches: BTreeSet<String>,
    node_subscribed: BTreeSet<String>,
    node_errors: BTreeMap<String, String>,
    ready: BTreeSet<String>,
    missing: BTreeSet<String>,
    operations: Vec<Operation>,
    retry_at: Instant,
    file_error: Option<String>,
    mount_error: Option<String>,
    service_error: Option<String>,
    pub wakeups: u64,
    pub reconnections: u64,
    initialized: bool,
}
fn owned(fd: i32) -> Result<OwnedFd> {
    if fd < 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}
impl ControlEvents {
    pub fn new() -> Result<Self> {
        Ok(Self {
            cooling: None,
            cooling_error: None,
            cooling_retry: Instant::now(),
            cooling_reconnections: 0,
            node_events: BTreeMap::new(),
            epoll: owned(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?,
            files: None,
            mounts: None,
            child: None,
            service_deadline: None,
            output: None,
            pending: vec![],
            watches: BTreeMap::new(),
            mask_watches: BTreeSet::new(),
            node_watches: BTreeSet::new(),
            node_subscribed: BTreeSet::new(),
            node_errors: BTreeMap::new(),
            ready: BTreeSet::new(),
            missing: BTreeSet::new(),
            operations: vec![],
            retry_at: Instant::now(),
            file_error: None,
            mount_error: None,
            service_error: None,
            wakeups: 0,
            reconnections: 0,
            initialized: false,
        })
    }
    pub fn fd(&self) -> i32 {
        self.epoll.as_raw_fd()
    }
    fn add_fd(&self, fd: i32, token: u64, events: u32) -> Result<()> {
        let mut event = libc::epoll_event { events, u64: token };
        if unsafe { libc::epoll_ctl(self.fd(), libc::EPOLL_CTL_ADD, fd, &mut event) } < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
    fn watch(&mut self, h: &Hardware, path: &str, kind: Watch) -> Result<()> {
        let path = CString::new(h.path(path).as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let fd = self.files.as_ref().ok_or("file_listener_unavailable")?;
        let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), MASK) };
        if wd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        self.watches.insert(wd, kind);
        Ok(())
    }
    fn files_start(&mut self, h: &Hardware) -> Result<()> {
        let fd = owned(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) })?;
        self.add_fd(fd.as_raw_fd(), FILES, libc::EPOLLIN as u32)?;
        self.files = Some(fd);
        self.watch(h, MASKS, Watch::Masks)?;
        if h.fixture {
            self.watch(h, crate::STATE, Watch::FixtureMounts)?;
            self.watch(h, "/services", Watch::ServiceDirectory)?;
            self.sync_fixture_services(h);
        }
        self.sync_masks(h);
        self.sync_nodes(h);
        Ok(())
    }
    fn sync_fixture_services(&mut self, h: &Hardware) {
        for name in SERVICE_NAMES {
            if !self.ready.contains(name)
                && self
                    .watch(h, &format!("/services/{name}"), Watch::Service(name.into()))
                    .is_ok()
            {
                self.ready.insert(name.into());
                self.missing.remove(name);
            }
        }
    }
    fn sync_nodes(&mut self, h: &Hardware) {
        let wanted = self
            .operations
            .iter()
            .filter(|o| o.target == BOUNCE || o.family == "cooling")
            .map(|o| (o.id.clone(), o.target.clone()))
            .collect::<Vec<_>>();
        for (id, path) in wanted {
            if self.node_watches.contains(&id) || h.present(&path) == Ok(false) {
                continue;
            }
            match self.watch(h, &path, Watch::Node(id.clone())) {
                Ok(()) => {
                    self.node_watches.insert(id.clone());
                    self.node_subscribed.insert(id.clone());
                    self.node_errors.remove(&id);
                }
                Err(error) => {
                    self.node_errors.insert(id, error);
                }
            }
        }
    }
    pub fn hybrid(&self, op: &Operation) -> bool {
        self.files.is_some() && self.node_watches.contains(&op.id)
    }
    fn subscribed_changes(&mut self, changes: &mut Changes) {
        for id in std::mem::take(&mut self.node_subscribed) {
            changes.ids.entry(id).or_insert("node_subscribed");
            changes.health = true;
        }
    }
    pub fn service_discovery_available(&self) -> bool {
        self.child.is_some() && self.service_deadline.is_none() && self.service_error.is_none()
    }
    fn sync_masks(&mut self, h: &Hardware) {
        let sources = self
            .operations
            .iter()
            .filter_map(|o| match &o.method {
                Method::Bind { source } => Some((o.id.clone(), source.clone(), o.target.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (id, source, target) in sources {
            if !self.mask_watches.contains(&id)
                && self.watch(h, &source, Watch::Mask(id.clone())).is_ok()
            {
                self.mask_watches.insert(id.clone());
                // Host fixtures copy files instead of mounting the source inode.
                if h.fixture {
                    let _ = self.watch(h, &target, Watch::Mask(id));
                }
            }
        }
    }
    pub fn set_operations(&mut self, h: &Hardware, ops: &[Operation]) {
        if self.operations == ops {
            return;
        }
        self.operations = ops.to_vec();
        // Remove stale inode watches, without disturbing healthy service/mount listeners.
        let wanted = ops.iter().map(|o| o.id.as_str()).collect::<BTreeSet<_>>();
        let stale = self
            .watches
            .iter()
            .filter_map(|(wd, w)| match w {
                Watch::Mask(id) | Watch::Node(id) if !wanted.contains(id.as_str()) => Some(*wd),
                _ => None,
            })
            .collect::<Vec<_>>();
        for wd in stale {
            self.watches.remove(&wd);
            if let Some(fd) = &self.files {
                unsafe {
                    libc::inotify_rm_watch(fd.as_raw_fd(), wd as _);
                }
            }
        }
        self.mask_watches.retain(|id| wanted.contains(id.as_str()));
        self.node_watches.retain(|id| wanted.contains(id.as_str()));
        self.node_subscribed
            .retain(|id| wanted.contains(id.as_str()));
        self.node_errors
            .retain(|id, _| wanted.contains(id.as_str()));
        self.sync_masks(h);
        self.sync_nodes(h);
    }
    fn read_mounts(file: &mut File) -> Result<()> {
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let mut buf = [0u8; 8192];
        let mut total = 0;
        loop {
            let n = file.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok(());
            }
            total += n;
            if total > 4_194_304 {
                return Err("mount_table_too_large".into());
            }
        }
    }
    fn mounts_start(&mut self) -> Result<()> {
        let mut f = File::open("/proc/self/mounts").map_err(|e| e.to_string())?;
        // Subscribe first, then establish the current mount generation.
        self.add_fd(
            f.as_raw_fd(),
            MOUNTS,
            (libc::EPOLLPRI | libc::EPOLLERR) as u32,
        )?;
        Self::read_mounts(&mut f)?;
        self.mounts = Some(f);
        Ok(())
    }
    fn services_start(&mut self) -> Result<()> {
        let mut child = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
            .arg("service-events")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let result = (|| {
            let output = child.stdout.take().ok_or("service_event_pipe_missing")?;
            let fd = output.as_raw_fd();
            if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            self.add_fd(fd, SERVICES, libc::EPOLLIN as u32)?;
            self.output = Some(output);
            Ok(())
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        } else {
            self.child = Some(child);
            self.service_deadline = Some(Instant::now() + Duration::from_secs(8));
        }
        result
    }
    fn stop_services(&mut self) {
        self.output = None;
        self.service_deadline = None;
        if let Some(mut child) = self.child.take() {
            drop(child.stdin.take());
            let _ = child.kill();
            let _ = child.wait();
        }
        self.ready.clear();
        self.missing.clear();
        self.pending.clear();
    }
    fn reset_files(&mut self, h: &Hardware, reason: &str) {
        self.files = None;
        self.watches.clear();
        self.mask_watches.clear();
        self.node_watches.clear();
        self.node_subscribed.clear();
        self.node_errors.clear();
        if h.fixture {
            self.ready.clear();
        }
        self.file_error = Some(reason.into());
        self.retry_at = Instant::now() + Duration::from_secs(5);
    }
    pub fn reconnect(&mut self, h: &Hardware) -> Changes {
        let mut changes = Changes::default();
        let wants_cooling = self.operations.iter().any(|o| o.family == "cooling");
        if !wants_cooling {
            self.cooling = None;
            self.cooling_error = None;
        } else {
            self.drain_cooling(&mut changes);
            if self.cooling.is_none() && Instant::now() >= self.cooling_retry {
                let result = crate::cooling_events::CoolingEvents::start(h).and_then(|listener| {
                    self.add_fd(listener.fd(), COOLING, libc::EPOLLIN as u32)?;
                    Ok(listener)
                });
                match result {
                    Ok(listener) => {
                        self.cooling = Some(listener);
                        self.cooling_error = None;
                        self.cooling_reconnections += 1;
                    }
                    Err(error) => {
                        self.cooling_error = Some(error);
                        self.cooling_retry = Instant::now() + Duration::from_secs(5);
                    }
                }
                changes.health = true;
            }
        }
        if self
            .service_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            for name in &self.ready {
                changes.ids.insert(name.clone(), "service_listener_lost");
            }
            self.stop_services();
            self.service_error = Some("service_listener_initialization_timeout".into());
            self.retry_at = Instant::now() + Duration::from_secs(5);
            changes.health = true;
        }
        if Instant::now() < self.retry_at {
            self.subscribed_changes(&mut changes);
            return changes;
        }
        let mut connected = false;
        if self.files.is_none() {
            match self.files_start(h) {
                Ok(()) => {
                    self.file_error = None;
                    changes.bindings = true;
                    connected = true;
                }
                Err(e) => self.reset_files(h, &e),
            }
        }
        if !h.fixture && self.mounts.is_none() {
            match self.mounts_start() {
                Ok(()) => {
                    self.mount_error = None;
                    changes.bindings = true;
                    connected = true;
                }
                Err(e) => self.mount_error = Some(e),
            }
        }
        if !h.fixture && self.child.is_none() {
            match self.services_start() {
                Ok(()) => {
                    self.service_error = None;
                    connected = true;
                }
                Err(e) => self.service_error = Some(e),
            }
        }
        if connected && self.initialized {
            self.reconnections += 1;
        }
        self.initialized = true;
        if self.files.is_some() {
            self.sync_nodes(h);
        }
        self.subscribed_changes(&mut changes);
        changes.health = true;
        self.retry_at = Instant::now() + Duration::from_secs(5);
        changes
    }
    pub fn retry_in(&self, fixture: bool) -> Option<Duration> {
        let retry = (self.files.is_none()
            || !self.node_errors.is_empty()
            || (!fixture && (self.mounts.is_none() || self.child.is_none())))
        .then_some(self.retry_at);
        retry
            .into_iter()
            .chain(self.service_deadline)
            .chain(
                self.operations
                    .iter()
                    .any(|o| o.family == "cooling")
                    .then(|| {
                        self.cooling.as_ref().map_or(self.cooling_retry, |c| {
                            Instant::now() + Duration::from_millis(c.wait_ms())
                        })
                    }),
            )
            .min()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }
    pub fn supports(&self, h: &Hardware, op: &Operation) -> bool {
        match op.method {
            Method::Bind { .. } => {
                self.files.is_some()
                    && (h.fixture || self.mounts.is_some())
                    && self.mask_watches.contains(&op.id)
            }
            Method::Service => self.ready.contains(&op.target),
            Method::Write if op.target == BOUNCE => self.hybrid(op),
            Method::Write if op.family == "cooling" => {
                self.hybrid(op) && self.cooling.as_ref().is_some_and(|c| c.ready())
            }
            _ => false,
        }
    }
    pub fn reason(&self, op: &Operation) -> &'static str {
        match op.method {
            Method::Bind { .. } => "挂载或掩码监听尚未就绪",
            Method::Service => "服务属性不存在或事件监听尚未就绪",
            Method::Shell | Method::Emulation => "保留已生效的周期写入机制",
            _ if op.family == "cooling" => "Cooling 通知源未全部就绪，临时保留原周期核验",
            _ if op.target.starts_with("/proc/game_opt/") => {
                "缺少可靠的私有 QoS 状态验证接口，保留定时续写"
            }
            _ if op.target == BOUNCE => "节点文件监听暂不可用，局部恢复周期核验",
            _ => "尚未验证全部状态修改来源的通知覆盖",
        }
    }
    pub fn observe_statuses(&mut self, h: &Hardware, statuses: &[BackendStatus]) {
        if !h.fixture
            && statuses.iter().any(|s| {
                s.family == "services" && s.state == "applied" && self.missing.contains(&s.id)
            })
        {
            self.stop_services();
            self.retry_at = Instant::now();
        }
    }
    pub fn health(&self) -> serde_json::Value {
        serde_json::json!({"mask_listener":self.files.is_some(),"mount_listener":self.mounts.is_some(),
            "cooling":self.cooling.as_ref().map(|c|c.health()),"cooling_error":self.cooling_error,
            "cooling_reconnections":self.cooling_reconnections,"node_events_by_id":self.node_events,
            "node_watches":self.node_watches,"node_errors":self.node_errors,"missing_services":self.missing,
            "service_properties":self.ready,"file_error":self.file_error,"mount_error":self.mount_error,
            "service_error":self.service_error,"wakeups":self.wakeups,"reconnections":self.reconnections})
    }
    pub fn drain(&mut self, h: &Hardware) -> Changes {
        let mut changes = Changes::default();
        let mut notifications = [libc::epoll_event { events: 0, u64: 0 }; 8];
        let n = unsafe { libc::epoll_wait(self.fd(), notifications.as_mut_ptr(), 8, 0) };
        if n <= 0 {
            self.subscribed_changes(&mut changes);
            return changes;
        }
        self.wakeups += 1;
        for notification in &notifications[..n as usize] {
            match notification.u64 {
                FILES => self.drain_files(h, &mut changes),
                MOUNTS => {
                    changes.bindings = true;
                    if notification.events & libc::EPOLLHUP as u32 != 0 {
                        self.mounts = None;
                        self.mount_error = Some("mount_listener_closed".into());
                        self.retry_at = Instant::now() + Duration::from_secs(5);
                        changes.health = true;
                    }
                    if let Some(f) = &mut self.mounts {
                        if let Err(e) = Self::read_mounts(f) {
                            self.mount_error = Some(e);
                            self.mounts = None;
                            changes.health = true;
                            self.retry_at = Instant::now() + Duration::from_secs(5);
                        }
                    }
                }
                SERVICES => self.drain_services(&mut changes),
                COOLING => self.drain_cooling(&mut changes),
                _ => (),
            }
        }
        self.subscribed_changes(&mut changes);
        changes
    }
    fn drain_files(&mut self, h: &Hardware, changes: &mut Changes) {
        let Some(fd) = self.files.as_ref().map(AsRawFd::as_raw_fd) else {
            return;
        };
        let mut buf = [0u8; 16384];
        // Bound a burst so that camera and stop events cannot be starved.
        for _ in 0..16 {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::WouldBlock
                    && e.kind() != std::io::ErrorKind::Interrupted
                {
                    self.reset_files(h, &e.to_string());
                    changes.health = true;
                    changes.bindings = true;
                }
                break;
            }
            if n == 0 {
                break;
            }
            let mut at = 0;
            while at + std::mem::size_of::<libc::inotify_event>() <= n as usize {
                let e = unsafe {
                    std::ptr::read_unaligned(buf[at..].as_ptr().cast::<libc::inotify_event>())
                };
                let start = at + std::mem::size_of::<libc::inotify_event>();
                let end = start + e.len as usize;
                if end > n as usize || e.mask & libc::IN_Q_OVERFLOW != 0 {
                    self.reset_files(h, "file_event_overflow_or_truncated");
                    changes.health = true;
                    changes.bindings = true;
                    return;
                }
                let name = std::str::from_utf8(
                    buf[start..end]
                        .split(|b| *b == 0)
                        .next()
                        .unwrap_or_default(),
                )
                .unwrap_or("");
                let kind = self.watches.get(&e.wd).cloned();
                let invalid =
                    e.mask & (libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0;
                match kind {
                    Some(Watch::Mask(id)) => {
                        changes.ids.insert(id.clone(), "mask_event");
                        if invalid {
                            self.mask_watches.remove(&id);
                            self.watches.remove(&e.wd);
                        }
                    }
                    Some(Watch::Node(id)) => {
                        // inotify has no writer identity. Coalesce and rate-limit, never discard presumed self events.
                        changes.ids.insert(id.clone(), "node_file_event");
                        *self.node_events.entry(id.clone()).or_default() += 1;
                        let receipts = changes.receipts.entry(id.clone()).or_default();
                        if receipts.len() < 8 {
                            receipts.push(serde_json::json!({"source":"node_inotify","received_ms":crate::runtime::now_ms(),"mask":e.mask,"writer":"unknown"}));
                        }
                        if invalid {
                            self.node_watches.remove(&id);
                            self.node_subscribed.remove(&id);
                            self.watches.remove(&e.wd);
                            self.node_errors.insert(id, "node_watch_lost".into());
                            self.retry_at = Instant::now() + Duration::from_secs(5);
                            changes.health = true;
                        }
                    }
                    Some(Watch::Masks) => {
                        if invalid {
                            self.reset_files(h, "mask_directory_lost");
                            changes.health = true;
                            changes.bindings = true;
                            return;
                        }
                        for op in &self.operations {
                            if let Method::Bind { source } = &op.method {
                                if source.rsplit('/').next() == Some(name) {
                                    changes.ids.insert(op.id.clone(), "mask_event");
                                }
                            }
                        }
                    }
                    Some(Watch::Service(service)) => {
                        if name == "state" || invalid {
                            changes.ids.insert(service, "service_event");
                        }
                        if invalid {
                            self.reset_files(h, "service_watch_lost");
                            changes.health = true;
                            return;
                        }
                    }
                    Some(Watch::ServiceDirectory) => {
                        self.sync_fixture_services(h);
                        for service in SERVICE_NAMES {
                            changes.ids.insert(service.into(), "service_discovery");
                        }
                        if invalid {
                            self.reset_files(h, "service_directory_lost");
                        }
                    }
                    Some(Watch::FixtureMounts) if name == "fixture-mounts.json" => {
                        changes.bindings = true
                    }
                    _ => (),
                }
                at = end;
            }
        }
        self.sync_masks(h);
    }
    fn drain_services(&mut self, changes: &mut Changes) {
        let mut buf = [0u8; 2048];
        let mut lost = false;
        for _ in 0..16 {
            let Some(output) = &mut self.output else {
                break;
            };
            match output.read(&mut buf) {
                Ok(0) => {
                    lost = true;
                    break;
                }
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    lost = true;
                    break;
                }
            }
            while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let line = self.pending.drain(..=end).collect::<Vec<_>>();
                let parts = std::str::from_utf8(&line)
                    .unwrap_or("")
                    .split_whitespace()
                    .collect::<Vec<_>>();
                if parts.len() != 3 || parts[0] != "CGS1" || !SERVICE_NAMES.contains(&parts[2]) {
                    lost = true;
                    break;
                }
                let name = parts[2].to_owned();
                match parts[1] {
                    "ready" => {
                        self.missing.remove(&name);
                        self.ready.insert(name.clone());
                        changes.health = true;
                        changes.ids.insert(name, "service_subscribed");
                    }
                    "changed" => {
                        changes.ids.insert(name, "service_event");
                    }
                    "missing" => {
                        self.missing.insert(name);
                        changes.health = true;
                    }
                    _ => {
                        lost = true;
                        break;
                    }
                }
                if self.ready.len() + self.missing.len() == SERVICE_NAMES.len() {
                    self.service_deadline = None;
                }
            }
            if self.pending.len() > 4096 || lost {
                lost = true;
                break;
            }
        }
        if lost {
            for name in &self.ready {
                changes.ids.insert(name.clone(), "service_listener_lost");
            }
            self.stop_services();
            self.service_error = Some("service_listener_disconnected".into());
            self.retry_at = Instant::now() + Duration::from_secs(5);
            changes.health = true;
        }
    }
    fn drain_cooling(&mut self, changes: &mut Changes) {
        let error = self
            .cooling
            .as_mut()
            .and_then(|c| c.drain(&self.operations, changes).err());
        if let Some(error) = error {
            self.cooling = None;
            self.cooling_error = Some(error);
            self.cooling_retry = Instant::now() + Duration::from_secs(5);
            changes.health = true;
            for op in self.operations.iter().filter(|o| o.family == "cooling") {
                changes.ids.insert(op.id.clone(), "cooling_listener_lost");
            }
        }
    }
}
impl Drop for ControlEvents {
    fn drop(&mut self) {
        self.stop_services();
    }
}

/// Internal native helper. stdin EOF ends every blocked waiter on parent death.
pub fn service_helper() -> Result<serde_json::Value> {
    #[cfg(target_os = "android")]
    {
        use std::io::Write;
        unsafe extern "C" {
            fn __system_property_find(name: *const libc::c_char) -> *const libc::c_void;
            fn __system_property_read_callback(
                pi: *const libc::c_void,
                callback: unsafe extern "C" fn(
                    *mut libc::c_void,
                    *const libc::c_char,
                    *const libc::c_char,
                    u32,
                ),
                cookie: *mut libc::c_void,
            );
            fn __system_property_wait(
                pi: *const libc::c_void,
                old: u32,
                new: *mut u32,
                timeout: *const libc::timespec,
            ) -> bool;
        }
        unsafe extern "C" fn capture_serial(
            cookie: *mut libc::c_void,
            _: *const libc::c_char,
            _: *const libc::c_char,
            serial: u32,
        ) {
            unsafe {
                *cookie.cast::<u32>() = serial;
            }
        }
        std::thread::Builder::new()
            .name("parent-lifetime".into())
            .spawn(|| {
                let mut b = [0u8; 1];
                loop {
                    if !matches!(std::io::stdin().read(&mut b), Ok(1)) {
                        std::process::exit(0);
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        fn spawn_waiter(name: &'static str, ptr: usize) -> Result<()> {
            std::thread::Builder::new()
                .name(format!("wait-{name}"))
                .spawn(move || {
                    let pi = ptr as *const libc::c_void;
                    let mut serial = 0u32;
                    unsafe {
                        __system_property_read_callback(
                            pi,
                            capture_serial,
                            (&mut serial as *mut u32).cast(),
                        );
                    }
                    println!("CGS1 ready {name}");
                    let _ = std::io::stdout().flush();
                    loop {
                        let mut next = serial;
                        if !unsafe {
                            __system_property_wait(pi, serial, &mut next, std::ptr::null())
                        } {
                            std::process::exit(1);
                        }
                        serial = next;
                        println!("CGS1 changed {name}");
                        let _ = std::io::stdout().flush();
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        // Snapshot the global generation before checking names; creation in the gap
        // then makes the blocking wait return immediately. No timed property polling.
        let mut generation = 0;
        unsafe {
            __system_property_wait(std::ptr::null(), 0, &mut generation, std::ptr::null());
        }
        let mut missing = vec![];
        for name in SERVICE_NAMES {
            let key = CString::new(format!("init.svc.{name}")).unwrap();
            let ptr = unsafe { __system_property_find(key.as_ptr()) } as usize;
            if ptr == 0 {
                println!("CGS1 missing {name}");
                missing.push(name);
            } else {
                spawn_waiter(name, ptr)?;
            }
        }
        if !missing.is_empty() {
            std::thread::Builder::new()
                .name("wait-service-creation".into())
                .spawn(move || {
                    while !missing.is_empty() {
                        let mut next = generation;
                        if !unsafe {
                            __system_property_wait(
                                std::ptr::null(),
                                generation,
                                &mut next,
                                std::ptr::null(),
                            )
                        } {
                            std::process::exit(1);
                        }
                        generation = next;
                        missing.retain(|name| {
                            let key = CString::new(format!("init.svc.{name}")).unwrap();
                            let ptr = unsafe { __system_property_find(key.as_ptr()) } as usize;
                            if ptr == 0 {
                                true
                            } else {
                                if spawn_waiter(name, ptr).is_err() {
                                    std::process::exit(1);
                                }
                                false
                            }
                        });
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        let _ = std::io::stdout().flush();
        loop {
            std::thread::park();
        }
    }
    #[cfg(not(target_os = "android"))]
    Err("native_property_wait_requires_android".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
    };

    static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    struct NodeFixture(Hardware);
    impl NodeFixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cg-node-events-{}-{}",
                std::process::id(),
                FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let h = Hardware::fixture(path);
            for dir in [MASKS, crate::STATE, "/services"] {
                fs::create_dir_all(h.path(dir)).unwrap();
            }
            Self(h)
        }
        fn put(&self, path: &str) {
            let target = self.0.path(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, "0").unwrap();
        }
    }
    impl Drop for NodeFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0.root);
        }
    }
    fn node(id: &str, target: &str) -> Operation {
        Operation {
            id: id.into(),
            family: if target == BOUNCE {
                "frequency"
            } else {
                "cooling"
            }
            .into(),
            target: target.into(),
            desired: "0".into(),
            method: Method::Write,
            reset: None,
            sensor_type: None,
        }
    }
    fn node_wd(listener: &ControlEvents, id: &str) -> i32 {
        *listener
            .watches
            .iter()
            .find(|(_, watch)| matches!(watch, Watch::Node(name) if name == id))
            .unwrap()
            .0
    }

    #[test]
    fn node_subscription_syncs_initial_and_local_watch_rebuild() {
        let fixture = NodeFixture::new();
        let h = &fixture.0;
        let peer = "/sys/class/thermal/cooling_device0/cur_state";
        fixture.put(BOUNCE);
        fixture.put(peer);
        let ops = [node("bouncing", BOUNCE), node(peer, peer)];
        let mut listener = ControlEvents::new().unwrap();
        listener.set_operations(h, &ops);
        let first = listener.reconnect(h);
        assert_eq!(first.ids.get("bouncing"), Some(&"node_subscribed"));
        assert_eq!(first.ids.get(peer), Some(&"node_subscribed"));
        assert!(listener.reconnect(h).ids.is_empty());
        let file_fd = listener.files.as_ref().unwrap().as_raw_fd();
        let peer_wd = node_wd(&listener, peer);
        fs::remove_file(h.path(BOUNCE)).unwrap();
        assert_eq!(
            listener.drain(h).ids.get("bouncing"),
            Some(&"node_file_event")
        );
        assert!(!listener.hybrid(&ops[0]));
        fixture.put(BOUNCE);
        listener.retry_at = Instant::now();
        let rebuilt = listener.reconnect(h);
        assert_eq!(rebuilt.ids.get("bouncing"), Some(&"node_subscribed"));
        assert!(!rebuilt.ids.contains_key(peer));
        assert!(listener.hybrid(&ops[0]));
        assert_eq!(listener.files.as_ref().unwrap().as_raw_fd(), file_fd);
        assert_eq!(node_wd(&listener, peer), peer_wd);
        assert!(listener.reconnect(h).ids.is_empty());
    }

    #[test]
    fn added_plan_node_syncs_before_retry_deadline() {
        let fixture = NodeFixture::new();
        let h = &fixture.0;
        fixture.put(BOUNCE);
        let bounce = node("bouncing", BOUNCE);
        let mut listener = ControlEvents::new().unwrap();
        listener.set_operations(h, &[bounce.clone()]);
        listener.reconnect(h);
        let peer = "/sys/class/thermal/cooling_device0/cur_state";
        fixture.put(peer);
        listener.set_operations(h, &[bounce, node(peer, peer)]);
        assert!(Instant::now() < listener.retry_at);
        let changed = listener.reconnect(h);
        assert_eq!(
            changed.ids,
            BTreeMap::from([(peer.into(), "node_subscribed")])
        );
        assert!(listener.reconnect(h).ids.is_empty());
    }

    #[test]
    fn failed_or_reset_watch_cannot_emit_stale_subscription_success() {
        let fixture = NodeFixture::new();
        let h = &fixture.0;
        let op = node("bouncing", BOUNCE);
        let mut listener = ControlEvents::new().unwrap();
        listener.set_operations(h, &[op.clone()]);
        assert!(!listener.reconnect(h).ids.contains_key("bouncing"));
        assert!(!listener.hybrid(&op));
        fixture.put(BOUNCE);
        listener.sync_nodes(h);
        assert!(listener.node_subscribed.contains("bouncing"));
        listener.reset_files(h, "fixture_overflow");
        assert!(listener.reconnect(h).ids.is_empty());
        assert!(!listener.hybrid(&op));
        listener.retry_at = Instant::now();
        assert_eq!(
            listener.reconnect(h).ids.get("bouncing"),
            Some(&"node_subscribed")
        );
        assert!(listener.hybrid(&op));
    }
    fn service() -> Operation {
        Operation {
            id: "thermal-engine".into(),
            family: "services".into(),
            target: "thermal-engine".into(),
            desired: "stopped".into(),
            method: Method::Service,
            reset: Some("running".into()),
            sensor_type: None,
        }
    }
    fn wait_service_change(listener: &mut ControlEvents, h: &Hardware) -> Changes {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut result = Changes::default();
        loop {
            let next = listener.drain(h);
            result.ids.extend(next.ids);
            result.health |= next.health;
            if result.ids.contains_key("thermal-engine") {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "service notification did not arrive"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn service_pipe_eof_removes_capability_and_schedules_reconnect() {
        let h = Hardware::fixture(std::env::temp_dir().join("unused-cg-listener-root"));
        let mut listener = ControlEvents::new().unwrap();
        let mut raw = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(raw.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) },
            0
        );
        let input = unsafe { OwnedFd::from_raw_fd(raw[0]) };
        let mut output = File::from(unsafe { OwnedFd::from_raw_fd(raw[1]) });
        listener
            .add_fd(input.as_raw_fd(), SERVICES, libc::EPOLLIN as u32)
            .unwrap();
        listener.output = Some(ChildStdout::from(input));
        output.write_all(b"CGS1 ready thermal-engine\n").unwrap();
        assert!(wait_service_change(&mut listener, &h)
            .ids
            .contains_key("thermal-engine"));
        assert!(listener.supports(&h, &service()));
        drop(output);
        // Readiness may be deferred during parallel child startup or signal delivery.
        let lost = wait_service_change(&mut listener, &h);
        assert!(lost.health && lost.ids.contains_key("thermal-engine"));
        assert!(!listener.supports(&h, &service()));
        assert_eq!(
            listener.health()["service_error"],
            "service_listener_disconnected"
        );
    }
    #[test]
    fn incomplete_service_handshake_has_a_bounded_deadline() {
        let h = Hardware::fixture(std::env::temp_dir().join("unused-cg-listener-root"));
        let mut listener = ControlEvents::new().unwrap();
        listener.ready.insert("thermal-engine".into());
        listener.service_deadline = Some(Instant::now() - Duration::from_secs(1));
        assert!(listener.reconnect(&h).health);
        assert!(!listener.supports(&h, &service()));
        assert_eq!(
            listener.health()["service_error"],
            "service_listener_initialization_timeout"
        );
    }
}
