//! Bounded, nonblocking child stdout. No reader thread can outlive a timeout.
use serde::Serialize;
use std::{
    io::{self, Read},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Valid,
    Timeout,
    ServiceExitFailure,
    ReadFailure,
    OutputLimit,
    FormatUnknown,
    SpawnFailure,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub outcome: Outcome,
    pub bytes_read: usize,
    pub intentional_truncation: bool,
    pub child_reaped: bool,
    pub exit_code: Option<i32>,
}
pub enum Progress {
    Continue,
    Complete,
    Invalid,
    Limit,
}

pub fn run<F: FnMut(&[u8]) -> Progress>(
    program: &str,
    args: &[&str],
    timeout_ms: u64,
    max: usize,
    mut consume: F,
) -> Report {
    let mut r = Report {
        outcome: Outcome::SpawnFailure,
        bytes_read: 0,
        intentional_truncation: false,
        child_reaped: false,
        exit_code: None,
    };
    let Ok(mut child) = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/system/bin:/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    else {
        return r;
    };
    let mut pipe = child.stdout.take().expect("piped stdout");
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    let start = Instant::now();
    let mut eof = false;
    r.outcome = if flags < 0
        || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        Outcome::ReadFailure
    } else {
        let mut buf = [0u8; 4096];
        loop {
            if start.elapsed() >= Duration::from_millis(timeout_ms) {
                break Outcome::Timeout;
            }
            // Read at most one byte beyond the budget; bounded even for an endless producer.
            let size = buf
                .len()
                .min(max.saturating_sub(r.bytes_read).saturating_add(1));
            match pipe.read(&mut buf[..size]) {
                Ok(0) => {
                    eof = true;
                    break Outcome::Valid;
                }
                Ok(n) => {
                    r.bytes_read += n;
                    if r.bytes_read > max {
                        break Outcome::OutputLimit;
                    }
                    match consume(&buf[..n]) {
                        Progress::Continue => (),
                        Progress::Complete => {
                            r.intentional_truncation = true;
                            break Outcome::Valid;
                        }
                        Progress::Invalid => {
                            r.intentional_truncation = true;
                            break Outcome::FormatUnknown;
                        }
                        Progress::Limit => {
                            r.intentional_truncation = true;
                            break Outcome::OutputLimit;
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let left = Duration::from_millis(timeout_ms).saturating_sub(start.elapsed());
                    let mut p = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let rc = unsafe { libc::poll(&mut p, 1, left.as_millis().min(20) as i32) };
                    if rc < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                        break Outcome::ReadFailure;
                    }
                }
                Err(_) => break Outcome::ReadFailure,
            }
        }
    };
    // WNOWAIT keeps the leader unreaped, reserving its process-group ID until
    // cleanup. Descendants retaining stdout are killed too; no detached readers.
    let exited = |pid: u32| -> io::Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { info.si_pid() } != 0)
        }
    };
    let mut was_exited = exited(child.id()).unwrap_or(false);
    if eof && !was_exited {
        while start.elapsed() < Duration::from_millis(timeout_ms) {
            match exited(child.id()) {
                Ok(true) => {
                    was_exited = true;
                    break;
                }
                Ok(false) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    r.outcome = Outcome::ReadFailure;
                    break;
                }
            }
        }
        if !was_exited && r.outcome == Outcome::Valid {
            r.outcome = Outcome::Timeout;
        }
    }
    // Inspect status before our pipe close/SIGKILL; induced termination is not a
    // service failure. No program-controlled exit status is claimed for truncation.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    drop(pipe);
    if let Ok(s) = child.wait() {
        r.child_reaped = true;
        if was_exited {
            r.exit_code = s.code();
            if !s.success() && r.outcome == Outcome::Valid {
                r.outcome = Outcome::ServiceExitFailure;
            }
        }
    }
    if !r.child_reaped {
        r.outcome = Outcome::ReadFailure;
    }
    r
}

pub fn text(program: &str, args: &[&str], timeout_ms: u64, max: usize) -> crate::Result<String> {
    let mut out = Vec::new();
    let r = run(program, args, timeout_ms, max, |b| {
        out.extend_from_slice(b);
        Progress::Continue
    });
    if r.outcome != Outcome::Valid {
        return Err(format!("command_{:?}", r.outcome));
    }
    String::from_utf8(out)
        .map(|s| s.trim().to_owned())
        .map_err(|_| "command_invalid_utf8".into())
}
