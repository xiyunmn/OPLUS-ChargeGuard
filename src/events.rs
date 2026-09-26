//! Blocking wakeups for power-supply changes, lifecycle files and signals.
use crate::{hardware::Hardware, Result};
use std::{
    ffi::CString,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::ffi::OsStrExt,
    sync::atomic::{AtomicI32, Ordering},
    time::{Duration, Instant},
};
static WAKE: AtomicI32 = AtomicI32::new(-1);
// eventfd write is async-signal-safe; a full counter already guarantees a wakeup.
pub fn notify() {
    let fd = WAKE.load(Ordering::Relaxed);
    if fd >= 0 {
        let one = 1u64;
        unsafe {
            libc::write(fd, (&one as *const u64).cast(), 8);
        }
    }
}
#[derive(Default)]
pub struct Wake {
    pub power: bool,
    pub changed: bool,
    pub discovery: bool,
}
pub struct Events {
    wake: OwnedFd,
    files: Option<OwnedFd>,
    power: Option<OwnedFd>,
    h: Hardware,
    wants_power: bool,
    retry_at: Instant,
    pub power_error: Option<String>,
    pub file_error: Option<String>,
    pub reconnects: u64,
}
fn owned(fd: i32) -> Result<OwnedFd> {
    if fd < 0 {
        Err(io::Error::last_os_error().to_string())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}
impl Events {
    pub fn new(h: &Hardware, power: bool) -> Result<Self> {
        let wake = owned(unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) })?;
        WAKE.store(wake.as_raw_fd(), Ordering::Relaxed);
        let mut events = Self {
            wake,
            files: None,
            power: None,
            h: h.clone(),
            wants_power: power,
            retry_at: Instant::now(),
            power_error: None,
            file_error: None,
            reconnects: 0,
        };
        events.reconnect();
        Ok(events)
    }
    fn files_socket(&self) -> Result<OwnedFd> {
        let fd = owned(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) })?;
        let mut paths = vec![self.h.state(), self.h.path(crate::MODULE)];
        if self.wants_power && self.h.fixture {
            paths.push(self.h.path("/sys/class/power_supply/battery"));
            paths.push(self.h.path("/sys/class/thermal"));
        }
        for path in paths {
            // Fixture modules may omit the installed module directory entirely.
            if self.h.fixture && !path.exists() {
                continue;
            }
            let path = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
            if unsafe {
                libc::inotify_add_watch(
                    fd.as_raw_fd(),
                    path.as_ptr(),
                    libc::IN_CLOSE_WRITE
                        | libc::IN_MOVED_TO
                        | libc::IN_CREATE
                        | libc::IN_DELETE
                        | libc::IN_DELETE_SELF
                        | libc::IN_MOVE_SELF,
                )
            } < 0
            {
                return Err(io::Error::last_os_error().to_string());
            }
        }
        Ok(fd)
    }
    fn reconnect(&mut self) -> Wake {
        let mut wake = Wake::default();
        if self.files.is_none() {
            match self.files_socket() {
                Ok(fd) => {
                    self.files = Some(fd);
                    self.file_error = None;
                    wake.changed = true;
                    if self.h.fixture && self.wants_power {
                        wake.power = true;
                    }
                }
                Err(e) => self.file_error = Some(e),
            }
        }
        if self.wants_power && !self.h.fixture && self.power.is_none() {
            match Self::power_socket() {
                Ok(fd) => {
                    self.power = Some(fd);
                    self.power_error = None;
                    self.reconnects += 1;
                    wake.power = true;
                    wake.discovery = true;
                }
                Err(e) => self.power_error = Some(e),
            }
        }
        self.retry_at = Instant::now() + Duration::from_secs(5);
        wake
    }
    fn needs_reconnect(&self) -> bool {
        self.files.is_none() || (self.wants_power && !self.h.fixture && self.power.is_none())
    }
    pub fn health(&self) -> serde_json::Value {
        serde_json::json!({"power_connected":self.power_available(),"power_error":self.power_error,"file_error":self.file_error,"reconnections":self.reconnects.saturating_sub(1),"periodic_charge_reads":false})
    }
    fn power_socket() -> Result<OwnedFd> {
        let fd = owned(unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        })?;
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        addr.nl_groups = 1;
        let size = 256 * 1024i32;
        unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&size as *const i32).cast(),
                4,
            );
        }
        if unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_nl).cast(),
                std::mem::size_of_val(&addr) as u32,
            )
        } < 0
        {
            return Err(io::Error::last_os_error().to_string());
        }
        Ok(fd)
    }
    pub fn power_available(&self) -> bool {
        self.power.is_some() || (self.wants_power && self.h.fixture && self.files.is_some())
    }
    pub fn wait(&mut self, duration: Duration, child: Option<i32>) -> Wake {
        let deadline = Instant::now() + duration;
        loop {
            if self.needs_reconnect() && Instant::now() >= self.retry_at {
                let wake = self.reconnect();
                if wake.power || wake.changed {
                    return wake;
                }
            }
            let mut fds = [
                self.wake.as_raw_fd(),
                self.files.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                self.power.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                child.unwrap_or(-1),
            ]
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
            let next = if self.needs_reconnect() {
                deadline.min(self.retry_at)
            } else {
                deadline
            };
            let left = next.saturating_duration_since(Instant::now());
            let rc = unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    fds.len() as libc::nfds_t,
                    left.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if rc < 0 {
                return Wake {
                    power: false,
                    changed: true,
                    discovery: false,
                };
            }
            if rc == 0 {
                if self.needs_reconnect() && Instant::now() >= self.retry_at {
                    continue;
                }
                return Wake::default();
            }
            let mut result = Wake::default();
            if fds[0].revents != 0 {
                let mut count = 0u64;
                unsafe {
                    libc::read(fds[0].fd, (&mut count as *mut u64).cast(), 8);
                }
                result.changed = true;
            }
            if fds[3].revents != 0 {
                result.changed = true;
            }
            if fds[1].revents != 0 {
                let mut buf = [0u8; 8192];
                for _ in 0..32 {
                    let n = unsafe { libc::read(fds[1].fd, buf.as_mut_ptr().cast(), buf.len()) };
                    if n <= 0 {
                        break;
                    }
                    let mut at = 0;
                    while at + std::mem::size_of::<libc::inotify_event>() <= n as usize {
                        let event = unsafe {
                            std::ptr::read_unaligned(
                                buf[at..].as_ptr().cast::<libc::inotify_event>(),
                            )
                        };
                        let start = at + std::mem::size_of::<libc::inotify_event>();
                        let end = start + event.len as usize;
                        if end > n as usize {
                            result.changed = true;
                            break;
                        }
                        let name = buf[start..end]
                            .split(|b| *b == 0)
                            .next()
                            .unwrap_or_default();
                        if self.wants_power && self.h.fixture && name == b"status" {
                            result.power = true;
                        }
                        if self.h.fixture
                            && (name.starts_with(b"thermal_zone")
                                || name.starts_with(b"cooling_device"))
                        {
                            result.discovery = true;
                        }
                        if event.mask
                            & (libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF)
                            != 0
                        {
                            self.files = None;
                            self.file_error = Some("file_watch_invalidated".into());
                            self.retry_at = Instant::now();
                        }
                        result.changed |= matches!(
                            name,
                            b"config.json"
                                | b"ownership.json"
                                | b"manual-stop"
                                | b"disable"
                                | b"remove"
                        ) || event.mask
                            & (libc::IN_Q_OVERFLOW
                                | libc::IN_IGNORED
                                | libc::IN_DELETE_SELF
                                | libc::IN_MOVE_SELF)
                            != 0;
                        at = end;
                    }
                }
            }
            if fds[1].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                self.files = None;
                self.file_error = Some("file_watch_error".into());
                self.retry_at = Instant::now();
                result.changed = true;
            }
            if fds[2].revents != 0 {
                let mut buf = [0u8; 65536];
                for _ in 0..64 {
                    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
                    let mut len = std::mem::size_of_val(&addr) as libc::socklen_t;
                    let n = unsafe {
                        libc::recvfrom(
                            fds[2].fd,
                            buf.as_mut_ptr().cast(),
                            buf.len(),
                            0,
                            (&mut addr as *mut libc::sockaddr_nl).cast(),
                            &mut len,
                        )
                    };
                    if n < 0 {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() == Some(libc::ENOBUFS) {
                            result.power = true;
                            result.discovery = true;
                        } else if error.kind() != io::ErrorKind::WouldBlock
                            && error.kind() != io::ErrorKind::Interrupted
                        {
                            self.power_error = Some(error.to_string());
                            self.power = None;
                            self.retry_at = Instant::now() + Duration::from_secs(5);
                            result.power = true;
                        }
                        break;
                    }
                    if n == 0 {
                        self.power = None;
                        self.power_error = Some("power_listener_closed".into());
                        self.retry_at = Instant::now() + Duration::from_secs(5);
                        result.power = true;
                        break;
                    }
                    if addr.nl_pid == 0 {
                        let packet = &buf[..n as usize];
                        result.power |= packet
                            .split(|b| *b == 0)
                            .any(|s| s == b"SUBSYSTEM=power_supply");
                        result.discovery |= packet.split(|b| *b == 0).any(|s| {
                            matches!(
                                s,
                                b"SUBSYSTEM=thermal"
                                    | b"SUBSYSTEM=platform"
                                    | b"SUBSYSTEM=module"
                                    | b"SUBSYSTEM=cpu"
                            )
                        }) && packet.split(|b| *b == 0).any(|s| {
                            matches!(
                                s,
                                b"ACTION=add"
                                    | b"ACTION=remove"
                                    | b"ACTION=move"
                                    | b"ACTION=bind"
                                    | b"ACTION=unbind"
                                    | b"ACTION=online"
                                    | b"ACTION=offline"
                            )
                        });
                    }
                }
            }
            if fds[2].revents & (libc::POLLHUP | libc::POLLNVAL) != 0 {
                self.power = None;
                self.power_error = Some("power_listener_lost".into());
                self.retry_at = Instant::now() + Duration::from_secs(5);
                result.power = true;
            }
            if result.power || result.changed || result.discovery || Instant::now() >= deadline {
                return result;
            }
        }
    }
}
impl Drop for Events {
    fn drop(&mut self) {
        WAKE.store(-1, Ordering::Relaxed);
    }
}
