//! Platform I/O. Fixture builds never invoke mount, Android services or properties.
use crate::{storage, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
};
pub const GAUGE: &str = "/sys/class/power_supply/battery/temp";
pub const MASKS: &str = "/dev/charge_guard_masks";
#[derive(Clone)]
pub struct Hardware {
    pub root: PathBuf,
    pub fixture: bool,
    counters: Arc<Counters>,
    capabilities: Arc<RwLock<Option<crate::capabilities::Capabilities>>>,
}
#[derive(Default)]
struct Counters {
    node_writes: AtomicU64,
    mask_writes: AtomicU64,
    service_requests: AtomicU64,
    getprop_processes: AtomicU64,
    mounts: AtomicU64,
    unmounts: AtomicU64,
    reads: AtomicU64,
    property_reads: AtomicU64,
    status_writes: AtomicU64,
}
#[derive(Serialize, Deserialize)]
struct FixtureMount {
    source: String,
    inode: u64,
}
impl Hardware {
    pub fn production() -> Result<Self> {
        #[cfg(target_os = "android")]
        {
            if unsafe { libc::geteuid() } != 0 {
                return Err("root_required".into());
            }
            Ok(Self {
                root: "/".into(),
                fixture: false,
                counters: Arc::default(),
                capabilities: Arc::default(),
            })
        }
        #[cfg(not(target_os = "android"))]
        {
            Err("host_requires_fixture_build".into())
        }
    }
    #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
    pub fn fixture(root: PathBuf) -> Self {
        Self {
            root,
            fixture: true,
            counters: Arc::default(),
            capabilities: Arc::default(),
        }
    }
    pub fn path(&self, p: &str) -> PathBuf {
        self.root.join(p.trim_start_matches('/'))
    }
    pub fn state(&self) -> PathBuf {
        self.path(crate::STATE)
    }
    pub fn read(&self, p: &str) -> Result<String> {
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
        if self.fixture
            && p.ends_with("/emul_temp")
            && fs::metadata(self.path(p)).is_ok_and(|m| m.mode() & 0o444 == 0)
        {
            // Emulate sysfs store-only attributes even when the host test runs as root.
            return Err("fixture_write_only_node".into());
        }
        read(&self.path(p), 65536)
    }
    pub fn exists(&self, p: &str) -> bool {
        self.path(p).exists()
    }
    /// ENOENT is absence; permission/I/O errors must not disable retries.
    pub fn present(&self, p: &str) -> Result<bool> {
        match fs::metadata(self.path(p)) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("node_metadata:{e}")),
        }
    }
    pub fn capabilities(&self) -> crate::capabilities::Capabilities {
        let mut cached = self.capabilities.write().unwrap();
        cached
            .get_or_insert_with(|| crate::capabilities::Capabilities::discover(self))
            .clone()
    }
    pub fn refresh_capabilities(&self) {
        let mut cached = self.capabilities.write().unwrap();
        if cached
            .as_ref()
            .is_some_and(|c| c.evidence_stamp != crate::capabilities::evidence_stamp(self))
        {
            *cached = None;
        }
    }
    #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
    pub fn fixture_capabilities(&self, profile: crate::capabilities::Capabilities) {
        assert!(self.fixture);
        *self.capabilities.write().unwrap() = Some(profile);
    }
    pub fn status_written(&self) {
        self.counters.status_writes.fetch_add(1, Ordering::Relaxed);
    }
    pub fn entries(&self, p: &str) -> Vec<String> {
        let mut out = fs::read_dir(self.path(p))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        out.sort();
        out
    }
    pub fn canonical(&self, p: &str) -> Result<String> {
        let q = fs::canonicalize(self.path(p)).map_err(|e| e.to_string())?;
        let r = q
            .strip_prefix(&self.root)
            .map_err(|_| "path_outside_fixture")?;
        Ok(format!("/{}", r.to_string_lossy()))
    }
    pub fn record(&self, v: serde_json::Value) {
        if self.fixture {
            let _ = storage::log(&self.state(), &v);
        }
    }
    pub fn write(&self, p: &str, v: &str) -> Result<()> {
        #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
        if self.fixture && self.path(&format!("{p}.store-error")).exists() {
            // A rejected sysfs store still closes a writable fd and emits inotify.
            let file = OpenOptions::new()
                .write(true)
                .open(self.path(p))
                .map_err(|e| e.to_string())?;
            drop(file);
            return Err("fixture_store_failure_after_open".into());
        }
        #[cfg(any(test, feature = "fixtures"))]
        if self.fixture && self.path(&format!("{p}.fail")).exists() {
            return Err("fixture_write_failure".into());
        }
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(self.path(p))
            .map_err(|e| format!("open:{e}"))?;
        f.write_all(v.as_bytes())
            .map_err(|e| format!("write:{e}"))?;
        #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
        if self.fixture && p == crate::pps::ENABLED && matches!(v, "0" | "1") {
            // Model the helper's synchronous disable/restore contract, not OEM charging.
            let raw = self.read(crate::pps::STATUS)?;
            let mut fields = raw
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            for field in &mut fields {
                if field.starts_with("enabled=") {
                    *field = format!("enabled={v}");
                }
                if v == "0" && field.starts_with("applied=") {
                    *field = "applied=0".into();
                }
            }
            fs::write(self.path(crate::pps::STATUS), fields.join(" "))
                .map_err(|e| e.to_string())?;
        }
        self.counters.node_writes.fetch_add(1, Ordering::Relaxed);
        self.record(serde_json::json!({"kind":"write","path":p,"value":v}));
        Ok(())
    }
    pub fn prop(&self, key: &str) -> Result<String> {
        self.prop_with_timeout(key, 2000)
    }
    fn prop_with_timeout(&self, key: &str, timeout_ms: u64) -> Result<String> {
        if self.fixture {
            if let Some(name) = key.strip_prefix("init.svc.") {
                return self.read(&format!("/services/{name}/state"));
            }
            return self.read(&format!("/properties/{key}"));
        }
        self.counters
            .getprop_processes
            .fetch_add(1, Ordering::Relaxed);
        crate::command::text("/system/bin/getprop", &[key], timeout_ms.max(1), 1024)
    }
    pub fn service(&self, name: &str, running: bool) -> Result<()> {
        let desired = if running { "running" } else { "stopped" };
        if self.service_state(name)?.as_deref() == Some(desired) {
            return Ok(());
        }
        self.request_service(name, running)?;
        self.counters
            .service_requests
            .fetch_add(1, Ordering::Relaxed);
        self.record(serde_json::json!({"kind":"service","name":name,"running":running}));
        // One bounded confirmation per request: immediate, then 50 ms, then
        // 100 ms intervals. Native property reads do not spawn child processes.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        let mut delay = std::time::Duration::from_millis(50);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            if self.service_state(name)?.as_deref() == Some(desired) {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(delay.min(left));
            delay = std::time::Duration::from_millis(100);
        }
        Err(format!("service_transition_pending:{name}:{desired}"))
    }
    pub fn service_state(&self, name: &str) -> Result<Option<String>> {
        self.counters.property_reads.fetch_add(1, Ordering::Relaxed);
        if self.fixture {
            let path = format!("/services/{name}/state");
            return if self.present(&path)? {
                self.read(&path).map(Some)
            } else {
                Ok(None)
            };
        }
        #[cfg(target_os = "android")]
        {
            use std::ffi::CStr;
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
            }
            unsafe extern "C" fn capture(
                cookie: *mut libc::c_void,
                _: *const libc::c_char,
                value: *const libc::c_char,
                _: u32,
            ) {
                unsafe {
                    *cookie.cast::<String>() = CStr::from_ptr(value).to_string_lossy().into_owned();
                }
            }
            let key = CString::new(format!("init.svc.{name}")).map_err(|e| e.to_string())?;
            unsafe {
                *libc::__errno() = 0;
                let pi = __system_property_find(key.as_ptr());
                if pi.is_null() {
                    let error = *libc::__errno();
                    return if error == 0 || error == libc::ENOENT {
                        Ok(None)
                    } else {
                        Err(format!(
                            "property_lookup:{}",
                            std::io::Error::from_raw_os_error(error)
                        ))
                    };
                }
                let mut value = String::new();
                __system_property_read_callback(pi, capture, (&mut value as *mut String).cast());
                if value.is_empty() {
                    return Err("service_property_empty".into());
                }
                Ok(Some(value))
            }
        }
        #[cfg(not(target_os = "android"))]
        Err("native_service_state_requires_android".into())
    }
    fn request_service(&self, name: &str, running: bool) -> Result<()> {
        #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
        if self.fixture {
            // Fake init state lives outside the property namespace; never calls Android APIs.
            let state = format!("/services/{name}/state");
            if self.path(&format!("{state}.fail")).exists() {
                return Err("service_control_failed".into());
            }
            if !self.path(&format!("{state}.unconfirmed")).exists() {
                fs::write(
                    self.path(&state),
                    if running { "running" } else { "stopped" },
                )
                .map_err(|e| e.to_string())?;
            }
            return Ok(());
        }
        crate::command::text(
            if running {
                "/system/bin/start"
            } else {
                "/system/bin/stop"
            },
            &[name],
            2500,
            1024,
        )?;
        Ok(())
    }
    /// Join init's existing namespace in this worker/recovery process only.
    pub fn control_namespace(&self) -> Result<()> {
        if self.fixture {
            return Ok(());
        }
        let t = fs::File::open("/proc/1/ns/mnt").map_err(|e| e.to_string())?;
        let c = fs::File::open("/proc/self/ns/mnt").map_err(|e| e.to_string())?;
        if t.metadata().map_err(|e| e.to_string())?.ino()
            != c.metadata().map_err(|e| e.to_string())?.ino()
        {
            sys(unsafe { libc::setns(t.as_raw_fd(), libc::CLONE_NEWNS) })?;
        }
        Ok(())
    }
    pub fn prepare_masks(&self) -> Result<()> {
        let p = self.path(MASKS);
        storage::secure_dir(&p)?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        if !self.fixture {
            let _ = crate::command::text(
                "/system/bin/chcon",
                &["u:object_r:tmpfs:s0", MASKS],
                1500,
                1024,
            );
        }
        Ok(())
    }
    fn fixture_mounts(&self) -> BTreeMap<String, FixtureMount> {
        storage::load(&self.state().join("fixture-mounts.json"), 1048576).unwrap_or_default()
    }
    pub fn mounted(&self, target: &str, source: &str) -> bool {
        if self.fixture {
            return self.fixture_mounts().get(target).is_some_and(|x| {
                x.source == source
                    && fs::metadata(self.path(target)).is_ok_and(|m| m.ino() == x.inode)
            });
        }
        match (
            fs::metadata(self.path(target)),
            fs::metadata(self.path(source)),
        ) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    pub fn has_mount_at(&self, target: &str) -> Result<bool> {
        if self.fixture {
            return Ok(self.fixture_mounts().contains_key(target));
        }
        let canonical = fs::canonicalize(self.path(target)).map_err(|e| e.to_string())?;
        let expected = canonical.to_string_lossy();
        let mounts = fs::read_to_string("/proc/self/mountinfo").map_err(|e| e.to_string())?;
        Ok(mounts
            .lines()
            .any(|line| line.split_whitespace().nth(4) == Some(expected.as_ref())))
    }
    pub fn bind(&self, source: &str, target: &str) -> Result<()> {
        #[cfg(any(test, feature = "fixtures"))]
        if self.fixture && self.path(&format!("{target}.fail")).exists() {
            return Err("fixture_mount_failure".into());
        }
        if self.fixture {
            let mut m = self.fixture_mounts();
            if !m.contains_key(target) {
                fs::copy(self.path(target), self.path(&format!("{target}.original")))
                    .map_err(|e| e.to_string())?;
            }
            fs::copy(self.path(source), self.path(target)).map_err(|e| e.to_string())?;
            m.insert(
                target.into(),
                FixtureMount {
                    source: source.into(),
                    inode: fs::metadata(self.path(target))
                        .map_err(|e| e.to_string())?
                        .ino(),
                },
            );
            storage::json(&self.state().join("fixture-mounts.json"), &m)?;
        } else {
            if self.has_mount_at(target)? {
                return Err("mount_target_already_covered".into());
            }
            let s = cs(&self.path(source))?;
            let t = cs(&self.path(target))?;
            sys(unsafe {
                libc::mount(
                    s.as_ptr(),
                    t.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                )
            })?;
        }
        self.record(serde_json::json!({"kind":"mount","source":source,"target":target}));
        self.counters.mounts.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    /// In-place updates retain the inode of an existing bind mount.
    pub fn mask_value(&self, source: &str, target: &str, value: &str) -> Result<()> {
        let p = self.path(source);
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&p)
            .map_err(|e| e.to_string())?;
        f.write_all(value.as_bytes()).map_err(|e| e.to_string())?;
        self.counters.mask_writes.fetch_add(1, Ordering::Relaxed);
        self.record(
            serde_json::json!({"kind":"mask_value","source":source,"target":target,"value":value}),
        );
        if !self.fixture {
            let _ = crate::command::text(
                "/system/bin/chcon",
                &["u:object_r:tmpfs:s0", source],
                1500,
                1024,
            );
        }
        if self.fixture && self.mounted(target, source) {
            fs::copy(p, self.path(target)).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    pub fn unbind(&self, target: &str, source: &str) -> Result<()> {
        if !self.mounted(target, source) {
            if self.fixture {
                if self.fixture_mounts().contains_key(target) {
                    return Err("mount_restore_conflict".into());
                }
            } else if self.has_mount_at(target)? {
                return Err("mount_restore_conflict".into());
            }
            return Ok(());
        }
        if self.fixture {
            let orig = self.path(&format!("{target}.original"));
            fs::copy(&orig, self.path(target)).map_err(|e| e.to_string())?;
            let mut m = self.fixture_mounts();
            m.remove(target);
            storage::json(&self.state().join("fixture-mounts.json"), &m)?;
            let _ = fs::remove_file(orig);
        } else {
            let t = cs(&self.path(target))?;
            sys(unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH) })?;
        }
        self.record(serde_json::json!({"kind":"umount","target":target}));
        self.counters.unmounts.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    pub fn counters(&self) -> serde_json::Value {
        let c = &self.counters;
        serde_json::json!({
            "node_writes":c.node_writes.load(Ordering::Relaxed),
            "mask_writes":c.mask_writes.load(Ordering::Relaxed),
            "service_requests":c.service_requests.load(Ordering::Relaxed),
            "getprop_processes":c.getprop_processes.load(Ordering::Relaxed),
            "mounts":c.mounts.load(Ordering::Relaxed),
            "unmounts":c.unmounts.load(Ordering::Relaxed),
            "reads":c.reads.load(Ordering::Relaxed),
            "property_reads":c.property_reads.load(Ordering::Relaxed),
            "status_writes":c.status_writes.load(Ordering::Relaxed)
        })
    }
}
pub fn read(p: &Path, max: usize) -> Result<String> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(p)
        .map_err(|e| e.to_string())?;
    let mut s = String::new();
    f.take((max + 1) as u64)
        .read_to_string(&mut s)
        .map_err(|e| e.to_string())?;
    if s.len() > max {
        return Err("read_too_large".into());
    }
    Ok(s.trim().into())
}
fn cs(p: &Path) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes()).map_err(|e| e.to_string())
}
fn sys(rc: i32) -> Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}
