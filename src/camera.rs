use crate::{hardware::Hardware, runtime::now_ms};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::net::UnixStream,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Usage {
    Unknown,
    Idle,
    Busy,
}
#[derive(Clone, Debug, Serialize)]
pub struct Observation {
    pub usage: Usage,
    pub sampled_ms: u64,
    pub idle_since_ms: Option<u64>,
    pub reason: String,
    pub event_driven: bool,
}
impl Default for Observation {
    fn default() -> Self {
        Self {
            usage: Usage::Unknown,
            sampled_ms: 0,
            idle_since_ms: None,
            reason: "starting".into(),
            event_driven: false,
        }
    }
}
impl Observation {
    pub fn permits_horae(&self, now: u64) -> bool {
        now >= self.sampled_ms
            && (self.event_driven || now - self.sampled_ms <= 2500)
            && self.usage == Usage::Idle
            && self
                .idle_since_ms
                .is_some_and(|t| now.saturating_sub(t) >= 3000)
    }
    pub fn update(&mut self, usage: Usage, reason: String, now: u64) {
        let continuous = self.usage == Usage::Idle
            && now >= self.sampled_ms
            && (self.event_driven || now - self.sampled_ms <= 2500);
        self.idle_since_ms = if usage == Usage::Idle {
            if continuous {
                self.idle_since_ms
            } else {
                Some(now)
            }
        } else {
            None
        };
        self.usage = usage;
        self.sampled_ms = now;
        self.reason = reason;
    }
}
fn publish(state: &Arc<Mutex<Observation>>, usage: Usage, reason: &str) {
    if let Ok(mut observation) = state.lock() {
        observation.event_driven = true;
        observation.update(usage, reason.into(), now_ms());
    }
    crate::events::notify();
}
fn stopped(stop: &UnixStream, timeout: i32) -> bool {
    let mut fd = libc::pollfd {
        fd: stop.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut fd, 1, timeout) > 0 }
}
fn listen(h: &Hardware, state: &Arc<Mutex<Observation>>, stop: &UnixStream) -> bool {
    let error = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(h.state().join("camera-events.log"));
    let mut command = Command::new("/system/bin/app_process");
    command
        .args(["/system/bin", "com.chargeguard.CameraEvents"])
        .env(
            "CLASSPATH",
            h.path(&format!("{}/bin/cg-camera.jar", crate::MODULE)),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(error.map(Stdio::from).unwrap_or_else(|_| Stdio::null()));
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let mut output = child.stdout.take().expect("camera event stdout");
    let mut pending = Vec::new();
    let mut grace: Option<Instant> = None;
    let mut initializing = Some(Instant::now() + Duration::from_secs(8));
    let mut requested_stop = false;
    loop {
        let deadline = grace.or(initializing);
        let timeout = deadline.map_or(-1, |t| {
            t.saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32
        });
        let mut fds = [
            libc::pollfd {
                fd: output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[1].revents != 0 {
            requested_stop = true;
            break;
        }
        if fds[0].revents != 0 {
            let mut bytes = [0u8; 1024];
            let n = match output.read(&mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            pending.extend_from_slice(&bytes[..n]);
            while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                let line = pending.drain(..=end).collect::<Vec<_>>();
                let usage = match std::str::from_utf8(&line).unwrap_or("").trim() {
                    "CGCAM1 idle" => Some(Usage::Idle),
                    "CGCAM1 busy" => Some(Usage::Busy),
                    "CGCAM1 unknown" => Some(Usage::Unknown),
                    _ => None,
                };
                if let Some(usage) = usage {
                    publish(state, usage, "camera_availability_event");
                    if usage != Usage::Unknown {
                        initializing = None;
                    }
                    grace = if usage == Usage::Idle {
                        Some(Instant::now() + Duration::from_secs(3))
                    } else {
                        None
                    };
                }
            }
            if pending.len() > 4096 {
                break;
            }
        }
        if grace.is_some_and(|t| Instant::now() >= t) {
            grace = None;
            crate::events::notify();
        }
        if initializing.is_some_and(|t| Instant::now() >= t) {
            break;
        }
    }
    drop(child.stdin.take());
    let _ = child.kill();
    let _ = child.wait();
    publish(state, Usage::Unknown, "camera_listener_disconnected");
    requested_stop
}
#[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
fn fixture_listen(h: &Hardware, state: &Arc<Mutex<Observation>>, stop: &UnixStream) {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let raw = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    if raw < 0 {
        return;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let path = std::ffi::CString::new(h.path("/camera").as_os_str().as_bytes()).unwrap();
    unsafe {
        libc::inotify_add_watch(
            raw,
            path.as_ptr(),
            libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_DELETE,
        );
    }
    loop {
        let u = match h.read("/camera/usage").unwrap_or_default().as_str() {
            "idle" => Usage::Idle,
            "busy" => Usage::Busy,
            _ => Usage::Unknown,
        };
        publish(state, u, "fixture_camera_event");
        let mut timeout = if u == Usage::Idle { 3000 } else { -1 };
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stop.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            unsafe {
                libc::poll(fds.as_mut_ptr(), 2, timeout);
            }
            if fds[1].revents != 0 {
                return;
            }
            if fds[0].revents != 0 {
                let mut buf = [0u8; 4096];
                unsafe {
                    libc::read(raw, buf.as_mut_ptr().cast(), buf.len());
                }
                break;
            }
            crate::events::notify();
            timeout = -1;
        }
    }
}
pub struct Monitor {
    state: Arc<Mutex<Observation>>,
    stop: UnixStream,
    thread: Option<JoinHandle<()>>,
}
impl Monitor {
    pub fn start(h: Hardware) -> crate::Result<Self> {
        let state = Arc::new(Mutex::new(Observation::default()));
        let (stop, reader) = UnixStream::pair().map_err(|e| e.to_string())?;
        let s = state.clone();
        let handle = thread::Builder::new()
            .name("camera-events".into())
            .spawn(move || {
                #[cfg(any(test, all(feature = "fixtures", not(target_os = "android"))))]
                if h.fixture {
                    fixture_listen(&h, &s, &reader);
                    return;
                }
                loop {
                    if stopped(&reader, 0) || listen(&h, &s, &reader) {
                        break;
                    }
                    publish(&s, Usage::Unknown, "camera_listener_retry");
                    // Failed listener restart only, not a camera-state query timer.
                    if stopped(&reader, 5000) {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            state,
            stop,
            thread: Some(handle),
        })
    }
    pub fn snapshot(&self) -> Observation {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.stop.write_all(&[1]);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
