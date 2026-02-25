use crate::error::{MmapError, Result, checked_add};
use crate::mmap_file::HEADER_SIZE;
use crate::raw_entry::RawEntry;
use serde::Deserialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions, read_dir};
use std::io::Read;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricType {
    Counter,
    Gauge,
    Histogram,
    Summary,
}

impl MetricType {
    fn as_prometheus_type(self) -> &'static str {
        match self {
            MetricType::Counter => "counter",
            MetricType::Gauge => "gauge",
            MetricType::Histogram => "histogram",
            MetricType::Summary => "summary",
        }
    }

    fn from_file_prefix(s: &str) -> Result<Self> {
        match s {
            "counter" => Ok(Self::Counter),
            "gauge" => Ok(Self::Gauge),
            "histogram" => Ok(Self::Histogram),
            "summary" => Ok(Self::Summary),
            _ => Err(MmapError::Message(format!("unknown metric type '{s}'"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiprocessMode {
    Min,
    Max,
    Livesum,
    All,
    Liveall,
}

impl MultiprocessMode {
    fn from_file_part(s: &str) -> Result<Self> {
        match s {
            "min" => Ok(Self::Min),
            "max" => Ok(Self::Max),
            "livesum" => Ok(Self::Livesum),
            "all" => Ok(Self::All),
            "liveall" => Ok(Self::Liveall),
            _ => Err(MmapError::Message(format!(
                "unknown multiprocess mode '{s}'"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
struct FileParams {
    path: PathBuf,
    multiprocess_mode: MultiprocessMode,
    metric_type: MetricType,
    pid: String,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct EntryKey {
    json: String,
    pid: Option<String>,
}

#[derive(Debug, Clone)]
struct EntryMeta {
    mode: MultiprocessMode,
    metric_type: MetricType,
    value: f64,
}

#[derive(Debug, Deserialize)]
struct MetricText<'a> {
    family_name: &'a str,
    metric_name: &'a str,
    labels: Vec<&'a str>,
    values: Vec<Value>,
}

pub fn aggregate_dir_to_prometheus_text(dir: &str) -> Result<String> {
    let files = discover_metric_files(dir)?;
    aggregate_to_prometheus_text(&files)
}

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

fn discover_metric_files(dir: &str) -> Result<Vec<FileParams>> {
    let mut files = Vec::new();
    for entry in read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("db")) {
            continue;
        }
        if !entry.file_type()?.is_file() {
            continue;
        }
        files.push(parse_file_params(&path)?);
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn parse_file_params(path: &Path) -> Result<FileParams> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| MmapError::Message(format!("invalid filename '{}'", path.display())))?;

    // Match Ruby: split('_').map { |e| e.gsub(/-\d+$/, '') }
    let parts: Vec<String> = stem
        .split('_')
        .map(|p| strip_file_suffix_number(p).to_string())
        .collect();

    if parts.is_empty() {
        return Err(MmapError::Message(format!(
            "invalid metric filename '{}'",
            path.display()
        )));
    }

    let metric_type = MetricType::from_file_prefix(&parts[0])?;
    let (multiprocess_mode, pid) = match metric_type {
        MetricType::Gauge => {
            if parts.len() < 3 {
                return Err(MmapError::Message(format!(
                    "gauge filename missing mode/pid '{}'",
                    path.display()
                )));
            }
            let mode = MultiprocessMode::from_file_part(&parts[1])?;
            let pid = parts[2..].join("_");
            (mode, pid)
        }
        _ => (MultiprocessMode::All, String::new()),
    };

    Ok(FileParams {
        path: path.to_path_buf(),
        multiprocess_mode,
        metric_type,
        pid,
    })
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
    t.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| d.as_secs())
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

fn aggregate_to_prometheus_text(files: &[FileParams]) -> Result<String> {
    let mut merged: HashMap<EntryKey, EntryMeta> = HashMap::new();
    for file in files {
        let bytes = read_file(&file.path)?;
        if bytes.len() < HEADER_SIZE {
            continue;
        }

        let used = u32::from_ne_bytes(bytes[0..4].try_into().expect("slice len checked")) as usize;
        let used = if used == 0 { HEADER_SIZE } else { used };
        if used > bytes.len() {
            return Err(MmapError::InvalidUsed {
                used,
                len: bytes.len(),
            });
        }

        let mut pos = HEADER_SIZE;
        while pos + 4 <= used {
            let entry = match RawEntry::from_slice(&bytes[pos..used]) {
                Ok(entry) => entry,
                Err(_) => break,
            };
            let key = match std::str::from_utf8(entry.json()) {
                Ok(key) => key,
                Err(_) => break,
            };
            let pid_significant = is_pid_significant(file.metric_type, file.multiprocess_mode);
            let entry_key = EntryKey {
                json: key.to_owned(),
                pid: pid_significant.then(|| file.pid.clone()),
            };
            let incoming = EntryMeta {
                mode: file.multiprocess_mode,
                metric_type: file.metric_type,
                value: entry.value(),
            };
            merged
                .entry(entry_key)
                .and_modify(|current| merge_meta(current, &incoming))
                .or_insert(incoming);

            pos = match checked_add(pos, entry.total_len()) {
                Ok(next) => next,
                Err(_) => break,
            };
        }
    }

    render(merged)
}

fn merge_meta(current: &mut EntryMeta, incoming: &EntryMeta) {
    if incoming.metric_type == MetricType::Gauge {
        match incoming.mode {
            MultiprocessMode::Min => current.value = current.value.min(incoming.value),
            MultiprocessMode::Max => current.value = current.value.max(incoming.value),
            MultiprocessMode::Livesum => current.value += incoming.value,
            MultiprocessMode::All | MultiprocessMode::Liveall => current.value = incoming.value,
        }
    } else {
        current.value += incoming.value;
    }
}

fn is_pid_significant(metric_type: MetricType, mode: MultiprocessMode) -> bool {
    if metric_type != MetricType::Gauge {
        return false;
    }
    !matches!(
        mode,
        MultiprocessMode::Min | MultiprocessMode::Max | MultiprocessMode::Livesum
    )
}

fn read_file(path: &PathBuf) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut out = Vec::new();
    file.read_to_end(&mut out)?;
    Ok(out)
}

fn render(merged: HashMap<EntryKey, EntryMeta>) -> Result<String> {
    let mut rows: Vec<(EntryKey, EntryMeta)> = merged.into_iter().collect();
    rows.sort_by(|a, b| compare_keys(&a.0, &b.0));

    let mut out = String::new();
    let mut previous_family: Option<String> = None;

    for (key, meta) in rows {
        let parsed: MetricText<'_> = serde_json::from_str(&key.json)?;
        if parsed.labels.len() != parsed.values.len() {
            return Err(MmapError::Message(
                "labels and values must have same length".to_string(),
            ));
        }

        if previous_family.as_deref() != Some(parsed.family_name) {
            out.push_str("# HELP ");
            out.push_str(parsed.family_name);
            out.push_str(" Multiprocess metric\n");
            out.push_str("# TYPE ");
            out.push_str(parsed.family_name);
            out.push(' ');
            out.push_str(meta.metric_type.as_prometheus_type());
            out.push('\n');
            previous_family = Some(parsed.family_name.to_owned());
        }

        out.push_str(parsed.metric_name);
        append_labels(&mut out, &parsed, key.pid.as_deref());
        out.push(' ');
        out.push_str(&meta.value.to_string());
        out.push('\n');
    }

    Ok(out)
}

fn append_labels(out: &mut String, parsed: &MetricText<'_>, pid: Option<&str>) {
    let has_labels = !parsed.labels.is_empty() || pid.is_some();
    if !has_labels {
        return;
    }
    out.push('{');

    let mut wrote_any = false;
    for (label, value) in parsed.labels.iter().zip(parsed.values.iter()) {
        if wrote_any {
            out.push(',');
        }
        out.push_str(label);
        out.push_str("=\"");
        let value = match value {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out.push_str(&escape_label_value(&value));
        out.push('"');
        wrote_any = true;
    }

    if let Some(pid) = pid {
        if wrote_any {
            out.push(',');
        }
        out.push_str("pid=\"");
        out.push_str(pid);
        out.push('"');
    }

    out.push('}');
}

fn escape_label_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out
}

fn compare_keys(a: &EntryKey, b: &EntryKey) -> Ordering {
    match a.json.cmp(&b.json) {
        Ordering::Equal => a.pid.cmp(&b.pid),
        ord => ord,
    }
}

#[cfg(test)]
mod test {
    use super::{aggregate_dir_to_prometheus_text, gc_metric_files};
    use crate::mmap_file::MmapMetricStore;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    #[test]
    fn aggregate_counter_files_from_dir() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("counter_100-0.db");
        let p2 = dir.path().join("counter_101-0.db");
        let key = r#"["http_requests_total","http_requests_total",["method"],["GET"]]"#;

        let mut s1 = MmapMetricStore::open(&p1).unwrap();
        let mut s2 = MmapMetricStore::open(&p2).unwrap();
        s1.increment(key, 2.0).unwrap();
        s2.increment(key, 3.0).unwrap();
        s1.flush().unwrap();
        s2.flush().unwrap();

        let rendered = aggregate_dir_to_prometheus_text(dir.path().to_str().unwrap()).unwrap();

        assert!(rendered.contains("# TYPE http_requests_total counter"));
        assert!(rendered.contains("http_requests_total{method=\"GET\"} 5"));
    }

    #[test]
    fn aggregate_gauge_all_keeps_pid_label() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("gauge_all_worker_1-0.db");
        let p2 = dir.path().join("gauge_all_worker_2-0.db");
        let key = r#"["queue_depth","queue_depth",[],[]]"#;

        let mut s1 = MmapMetricStore::open(&p1).unwrap();
        let mut s2 = MmapMetricStore::open(&p2).unwrap();
        s1.set(key, 2.0).unwrap();
        s2.set(key, 3.0).unwrap();
        s1.flush().unwrap();
        s2.flush().unwrap();

        let rendered = aggregate_dir_to_prometheus_text(dir.path().to_str().unwrap()).unwrap();

        assert!(rendered.contains("queue_depth{pid=\"worker_1\"} 2"));
        assert!(rendered.contains("queue_depth{pid=\"worker_2\"} 3"));
    }

    #[test]
    fn slash_in_label_is_not_json_escaped() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("counter_200-0.db");
        let key = r#"["http_requests_total","http_requests_total",["path"],["/hello"]]"#;

        let mut s1 = MmapMetricStore::open(&p1).unwrap();
        s1.increment(key, 1.0).unwrap();
        s1.flush().unwrap();

        let rendered = aggregate_dir_to_prometheus_text(dir.path().to_str().unwrap()).unwrap();
        assert!(rendered.contains(r#"http_requests_total{path="/hello"} 1"#));
        assert!(!rendered.contains(r#"http_requests_total{path="\/hello"}"#));
    }

    #[test]
    fn aggregate_tolerates_corrupt_trailing_entry() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("counter_300-0.db");
        let key = r#"["http_requests_total","http_requests_total",["path"],["/ok"]]"#;

        let mut s1 = MmapMetricStore::open(&p1).unwrap();
        s1.increment(key, 2.0).unwrap();
        s1.flush().unwrap();

        let mut bytes = fs::read(&p1).unwrap();
        let used = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let trailing = [0xFF, 0xEE, 0xDD, 0xCC, 0xAA, 0xBB];
        bytes.extend_from_slice(&trailing);
        let new_used = used + trailing.len();
        bytes[0..4].copy_from_slice(&(new_used as u32).to_ne_bytes());
        fs::write(&p1, bytes).unwrap();

        let rendered = aggregate_dir_to_prometheus_text(dir.path().to_str().unwrap()).unwrap();
        assert!(rendered.contains(r#"http_requests_total{path="/ok"} 2"#));
    }

    #[test]
    fn gc_deletes_dead_numeric_pid_files_only() {
        let dir = tempdir().unwrap();
        let alive = dir.path().join(format!("counter_{}-0.db", std::process::id()));
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

    // Manual stress reproducer for potential weak-memory ordering races:
    // run explicitly on ARM:
    //   cargo test stress_append_vs_scrape_visibility -- --ignored --nocapture
    #[test]
    #[ignore = "stress reproducer; run explicitly"]
    fn stress_append_vs_scrape_visibility() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("counter_stress-0.db");
        let key_prefix = "stress_metric";
        let iters = std::env::var("PMMAP_STRESS_ITERS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(40_000);

        let done = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let errs = Arc::new(AtomicUsize::new(0));

        let writer_done = done.clone();
        let writer_writes = writes.clone();
        let writer_db = db.clone();
        let writer = thread::spawn(move || {
            let mut store = MmapMetricStore::open(&writer_db).unwrap();
            for i in 0..iters {
                let key = format!(r#"["{0}","{0}",["id"],["{1}"]]"#, key_prefix, i);
                let _ = store.increment(&key, 1.0);
                writer_writes.fetch_add(1, Ordering::Relaxed);

                // Yield occasionally to increase interleaving with reader.
                if (i % 128) == 0 {
                    thread::yield_now();
                }
            }
            writer_done.store(true, Ordering::Relaxed);
        });

        let reader_done = done.clone();
        let reader_reads = reads.clone();
        let reader_errs = errs.clone();
        let reader_dir = dir.path().to_string_lossy().to_string();
        let reader = thread::spawn(move || {
            while !reader_done.load(Ordering::Relaxed) {
                match aggregate_dir_to_prometheus_text(&reader_dir) {
                    Ok(_) => {}
                    Err(_) => {
                        reader_errs.fetch_add(1, Ordering::Relaxed);
                    }
                }
                reader_reads.fetch_add(1, Ordering::Relaxed);
            }
            // A few extra passes after writer completion.
            for _ in 0..64 {
                let _ = aggregate_dir_to_prometheus_text(&reader_dir);
                reader_reads.fetch_add(1, Ordering::Relaxed);
            }
        });

        let start = Instant::now();
        writer.join().unwrap();
        reader.join().unwrap();
        let elapsed = start.elapsed();

        let writes = writes.load(Ordering::Relaxed);
        let reads = reads.load(Ordering::Relaxed);
        let errs = errs.load(Ordering::Relaxed);
        eprintln!(
            "stress_append_vs_scrape_visibility: writes={writes} reads={reads} errs={errs} elapsed={:?}",
            elapsed
        );

        // The test itself is a reproducer harness; it always passes and reports
        // observed scrape errors so runs can be compared before/after fixes.
        assert!(writes > 0);
        assert!(reads > 0);
        // Keep runtime bounded in CI/manual runs.
        assert!(elapsed < Duration::from_secs(45));
    }
}
