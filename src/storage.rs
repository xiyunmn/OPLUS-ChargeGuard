use crate::Result;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Component, Path},
    sync::atomic::{AtomicU64, Ordering},
};
static SEQ: AtomicU64 = AtomicU64::new(1);
// Four full backups retain about 50 minutes at the observed peak charging log
// rate (~158 KiB/min); the current file brings the total bound to 10 MiB.
pub const DETAIL_LOG_FILE_BYTES: usize = 2 * 1024 * 1024;
pub const DETAIL_LOG_BACKUPS: usize = 4;
pub fn secure_dir(p: &Path) -> Result<()> {
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("unsafe_directory".into());
    }
    let mut cur = std::path::PathBuf::from("/");
    for c in p.components().skip(1) {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => return Err("symlink_or_non_directory".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&cur).map_err(|e| e.to_string())?;
                fs::set_permissions(&cur, fs::Permissions::from_mode(0o700))
                    .map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let m = fs::metadata(p).map_err(|e| e.to_string())?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err("insecure_state_owner_or_mode".into());
    }
    Ok(())
}
fn regular(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m)
            if m.is_file()
                && !m.file_type().is_symlink()
                && m.nlink() == 1
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o022 == 0 =>
        {
            Ok(())
        }
        Ok(_) => Err("unsafe_state_file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
pub fn read(p: &Path, max: usize) -> Result<String> {
    regular(p)?;
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(p)
        .map_err(|e| e.to_string())?;
    let mut b = Vec::new();
    (&mut f)
        .take((max + 1) as u64)
        .read_to_end(&mut b)
        .map_err(|e| e.to_string())?;
    if b.len() > max {
        return Err("file_too_large".into());
    }
    String::from_utf8(b).map_err(|_| "invalid_utf8".into())
}
pub fn atomic(p: &Path, data: &[u8]) -> Result<()> {
    let parent = p.parent().ok_or("missing_parent")?;
    secure_dir(parent)?;
    regular(p)?;
    let temp = parent.join(format!(
        ".txn-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        f.write_all(data)
            .and_then(|_| f.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&temp, p).map_err(|e| e.to_string())?;
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
pub struct Lock {
    _file: File,
}
impl Lock {
    pub fn take(p: &Path) -> Result<Self> {
        secure_dir(p.parent().ok_or("missing_parent")?)?;
        regular(p)?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(p)
            .map_err(|e| e.to_string())?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("owner_lock_busy".into());
        }
        Ok(Self { _file: f })
    }
}
pub fn json<T: serde::Serialize>(p: &Path, v: &T) -> Result<()> {
    let bytes = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    if read(p, bytes.len()).is_ok_and(|old| old.as_bytes() == bytes) {
        return Ok(());
    }
    atomic(p, &bytes)
}
pub fn load<T: serde::de::DeserializeOwned>(p: &Path, max: usize) -> Result<T> {
    serde_json::from_str(&read(p, max)?).map_err(|_| "invalid_json_schema".into())
}
pub fn log(dir: &Path, event: &serde_json::Value) -> Result<()> {
    append_rotating(dir, "events.jsonl", event, 262144, 4)
}
pub fn rotate(dir: &Path, name: &str, keep: usize) -> Result<()> {
    if name.contains('/') || name.contains('\\') || keep == 0 || keep > 10 {
        return Err("invalid_log_rotation".into());
    }
    for i in (1..=keep).rev() {
        let from = dir.join(if i == 1 {
            name.to_owned()
        } else {
            format!("{name}.{}", i - 1)
        });
        let to = dir.join(format!("{name}.{i}"));
        regular(&from)?;
        regular(&to)?;
        match fs::rename(&from, &to) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}
pub fn append_rotating(
    dir: &Path,
    name: &str,
    event: &serde_json::Value,
    limit: usize,
    keep: usize,
) -> Result<()> {
    if name.contains('/') || name.contains('\\') {
        return Err("invalid_log_name".into());
    }
    let mut b = serde_json::to_vec(event).map_err(|e| e.to_string())?;
    b.push(b'\n');
    if b.len() > limit {
        return Err("log_record_too_large".into());
    }
    let _lock = Lock::take(&dir.join(format!("{name}.lock")))?;
    let p = dir.join(name);
    regular(&p)?;
    match fs::metadata(&p) {
        Ok(m) if m.len().saturating_add(b.len() as u64) > limit as u64 => rotate(dir, name, keep)?,
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.to_string()),
        _ => (),
    }
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p)
        .map_err(|e| e.to_string())?;
    f.write_all(&b).map_err(|e| e.to_string())
}
