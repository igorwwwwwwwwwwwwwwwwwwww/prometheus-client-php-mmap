use crate::error::{MmapError, Result};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions, read_dir};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub fn gc_metric_files(
    dir: &str,
    budget_ms: u64,
    scan_limit: usize,
    delete_limit: usize,
    dead_grace_sec: u64,
) -> Result<usize> {
    let lock_path = Path::new(dir).join(".gc.lock");
    let state_path = Path::new(dir).join(".gc.cursor");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    let lock_guard = try_lock_exclusive_nonblocking(&lock_file)?;
    if lock_guard.is_none() {
        return Ok(0);
    }

    let mut files = Vec::<PathBuf>::new();
    for entry in read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("db")) {
            continue;
        }
        if !entry.file_type()?.is_file() {
            continue;
        }
        files.push(path);
    }
    if files.is_empty() {
        return Ok(0);
    }
    files.sort();

    let cursor = std::fs::read_to_string(&state_path)
        .ok()
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();

    let mut start_idx = 0usize;
    if !cursor.is_empty() {
        let cursor_path = Path::new(dir).join(&cursor);
        if let Some(pos) = files.iter().position(|p| p == &cursor_path) {
            start_idx = (pos + 1) % files.len();
        }
    }

    let mut processed = 0usize;
    let mut deleted = 0usize;
    let mut last_seen: Option<String> = None;
    let deadline = Instant::now() + Duration::from_millis(budget_ms.max(1));

    while processed < scan_limit && processed < files.len() {
        if Instant::now() >= deadline {
            break;
        }

        let idx = (start_idx + processed) % files.len();
        let path = &files[idx];
        let base = match path.file_name().and_then(|s| s.to_str()) {
            Some(base) => base,
            None => {
                processed += 1;
                continue;
            }
        };
        last_seen = Some(base.to_owned());
        processed += 1;

        let Some(pid) = extract_numeric_pid(base) else {
            continue;
        };
        if is_pid_alive(pid) {
            continue;
        }

        let mtime = match std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(seconds_since_epoch)
        {
            Some(ts) => ts,
            None => continue,
        };
        let now = match seconds_since_epoch(SystemTime::now()) {
            Some(ts) => ts,
            None => continue,
        };
        if now.saturating_sub(mtime) < dead_grace_sec {
            continue;
        }

        if std::fs::remove_file(path).is_ok() {
            deleted += 1;
            if deleted >= delete_limit {
                break;
            }
        }
    }

    if let Some(last_seen) = last_seen {
        let _ = std::fs::write(state_path, format!("{last_seen}\n"));
    }

    Ok(deleted)
}

fn strip_file_suffix_number(part: &str) -> &str {
    let bytes = part.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i > 0 && i < bytes.len() && bytes[i - 1] == b'-' {
        &part[..i - 1]
    } else {
        part
    }
}

fn extract_numeric_pid(filename: &str) -> Option<i32> {
    let stem = filename.strip_suffix(".db")?;
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.is_empty() {
        return None;
    }

    let pid_part = match parts[0] {
        "gauge" => {
            if parts.len() < 3 {
                return None;
            }
            parts[2..].join("_")
        }
        "counter" | "histogram" | "summary" => {
            if parts.len() < 2 {
                return None;
            }
            parts[1..].join("_")
        }
        _ => return None,
    };
    let pid_str = strip_file_suffix_number(&pid_part);
    if pid_str.is_empty() || !pid_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid_str.parse::<i32>().ok().filter(|pid| *pid > 0)
}

fn seconds_since_epoch(t: SystemTime) -> Option<u64> {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg(unix)]
fn is_pid_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: i32) -> bool {
    true
}

#[cfg(unix)]
struct FileLockGuard<'a> {
    file: &'a File,
}

#[cfg(unix)]
impl Drop for FileLockGuard<'_> {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(unix)]
fn try_lock_exclusive_nonblocking(file: &File) -> Result<Option<FileLockGuard<'_>>> {
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(FileLockGuard { file }));
    }
    let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    if code == libc::EWOULDBLOCK || code == libc::EAGAIN {
        return Ok(None);
    }
    Err(MmapError::Io(std::io::Error::last_os_error()))
}

#[cfg(not(unix))]
fn try_lock_exclusive_nonblocking(_file: &File) -> Result<Option<()>> {
    Ok(Some(()))
}

#[cfg(test)]
mod test {
    use super::gc_metric_files;
    use tempfile::tempdir;

    #[test]
    fn gc_deletes_dead_numeric_pid_files_only() {
        let dir = tempdir().unwrap();
        let alive = dir
            .path()
            .join(format!("counter_{}-0.db", std::process::id()));
        let dead = dir.path().join("counter_2000000000-0.db");
        let non_numeric = dir.path().join("gauge_all_worker_1-0.db");

        std::fs::write(&alive, [0u8; 8]).unwrap();
        std::fs::write(&dead, [0u8; 8]).unwrap();
        std::fs::write(&non_numeric, [0u8; 8]).unwrap();

        let deleted = gc_metric_files(dir.path().to_str().unwrap(), 100, 64, 64, 0).unwrap();
        assert!(deleted >= 1);
        assert!(alive.exists());
        assert!(!dead.exists());
        assert!(non_numeric.exists());
    }
}
