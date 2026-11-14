//! Single-writer lock for an archive folder.
//!
//! SQLite and Tantivy locking are unreliable over SMB/NFS, so the archive
//! folder itself never holds a live database. Writers instead take this lock:
//! a file created with `create_new` (atomic on local disks, SMB and NFSv3+),
//! holding who owns it. Readers never need it.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};

pub const LOCK_FILE: &str = "writer.lock";
/// A lock whose heartbeat is older than this is treated as abandoned.
pub const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

#[derive(Debug)]
pub struct WriterLock {
    path: PathBuf,
    last_beat: SystemTime,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct LockInfo {
    host: String,
    pid: u32,
    started: i64,
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".into())
}

impl WriterLock {
    pub fn acquire(archive_root: &Path, break_stale: bool) -> Result<WriterLock> {
        let path = archive_root.join(LOCK_FILE);
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let info = LockInfo {
                        host: hostname(),
                        pid: std::process::id(),
                        started: chrono::Utc::now().timestamp(),
                    };
                    f.write_all(serde_json::to_string(&info)?.as_bytes())?;
                    f.sync_all()?;
                    return Ok(WriterLock { path, last_beat: SystemTime::now() });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let owner = fs::read_to_string(&path).unwrap_or_default();
                    let age = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|m| SystemTime::now().duration_since(m).ok())
                        .unwrap_or_default();
                    let info: Option<LockInfo> = serde_json::from_str(&owner).ok();
                    let same_host_dead = info
                        .as_ref()
                        .map(|i| i.host == hostname() && !pid_alive(i.pid))
                        .unwrap_or(false);
                    if break_stale || age > STALE_AFTER || same_host_dead {
                        fs::remove_file(&path).ok();
                        continue;
                    }
                    bail!(
                        "archive is being written by another process ({}), last active {}s ago. \
                         Close it there, or pass --break-lock if it crashed.",
                        info.map(|i| format!("{} pid {}", i.host, i.pid)).unwrap_or(owner),
                        age.as_secs()
                    );
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        bail!("could not acquire writer lock at {}", path.display())
    }

    /// Refresh the lock's mtime so other machines can tell it is live.
    /// Cheap to call often; only touches the file once a minute.
    pub fn heartbeat(&mut self) {
        if self.last_beat.elapsed().unwrap_or_default() < Duration::from_secs(60) {
            return;
        }
        if let Ok(f) = OpenOptions::new().append(true).open(&self.path) {
            let _ = f.set_modified(SystemTime::now());
        }
        self.last_beat = SystemTime::now();
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    // `tasklist` is always present on Windows; avoids an extra crate.
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(true)
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        || std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_writer_is_refused_then_allowed_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let a = WriterLock::acquire(dir.path(), false).unwrap();
        assert!(WriterLock::acquire(dir.path(), false).is_err());
        drop(a);
        WriterLock::acquire(dir.path(), false).unwrap();
    }

    #[test]
    fn dead_owner_on_same_host_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let info = LockInfo { host: hostname(), pid: 0x7fff_fff0, started: 0 };
        fs::write(dir.path().join(LOCK_FILE), serde_json::to_string(&info).unwrap()).unwrap();
        WriterLock::acquire(dir.path(), false).unwrap();
    }
}
