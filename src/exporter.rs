use crate::aggregate::{MetricType, MultiprocessMode};
use crate::error::{MmapError, Result, checked_add};
use crate::metric_key::{StoredMetricKey, escape_label_value};
use crate::mmap_file::{FILE_FORMAT_VERSION, HEADER_SIZE};
use crate::raw_entry::RawEntry;
use memmap2::{Mmap, MmapOptions};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{File, Metadata, OpenOptions, read_dir};
use std::path::{Path, PathBuf};

pub struct CachedDirExporter {
    dir: PathBuf,
    files: HashMap<PathBuf, CachedFile>,
}

struct CachedFile {
    params: FileParams,
    file: File,
    map: Mmap,
    identity: Option<FileIdentity>,
    parsed_used: usize,
    entries: Vec<CachedEntry>,
    active: bool,
}

struct CachedEntry {
    metric: StoredMetricKey,
    value_offset: usize,
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
    metric: StoredMetricKey,
    pid: Option<String>,
}

#[derive(Debug, Clone)]
struct EntryMeta {
    mode: MultiprocessMode,
    metric_type: MetricType,
    value: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl CachedDirExporter {
    pub fn new(dir: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            dir: dir.as_ref().to_path_buf(),
            files: HashMap::new(),
        })
    }

    pub fn render(&mut self) -> Result<String> {
        let mut file_errors = self.refresh()?;

        let mut merged: HashMap<EntryKey, EntryMeta> = HashMap::new();
        let mut failed = HashSet::new();
        for (path, cached) in &mut self.files {
            if let Err(err) = cached.refresh_entries() {
                record_file_error(path, &err);
                file_errors += 1;
                failed.insert(path.clone());
                continue;
            }
            if !cached.active {
                continue;
            }

            let used = match cached.current_used() {
                Ok(used) => used,
                Err(err) => {
                    record_file_error(path, &err);
                    file_errors += 1;
                    failed.insert(path.clone());
                    continue;
                }
            };
            let pid_significant =
                is_pid_significant(cached.params.metric_type, cached.params.multiprocess_mode);
            for entry in &cached.entries {
                let Some(value_end) = entry.value_offset.checked_add(8) else {
                    record_file_error(path, "value offset overflow");
                    file_errors += 1;
                    failed.insert(path.clone());
                    break;
                };
                if value_end > used {
                    continue;
                }
                let value = match cached.read_value(entry.value_offset) {
                    Ok(value) => value,
                    Err(err) => {
                        record_file_error(path, &err);
                        file_errors += 1;
                        failed.insert(path.clone());
                        break;
                    }
                };
                let incoming = EntryMeta {
                    mode: cached.params.multiprocess_mode,
                    metric_type: cached.params.metric_type,
                    value,
                };
                let entry_key = EntryKey {
                    metric: entry.metric.clone(),
                    pid: pid_significant.then(|| cached.params.pid.clone()),
                };
                merged
                    .entry(entry_key)
                    .and_modify(|current| merge_meta(current, &incoming))
                    .or_insert(incoming);
            }
        }

        self.files.retain(|path, _| !failed.contains(path));
        render(merged, file_errors)
    }

    fn refresh(&mut self) -> Result<usize> {
        let (discovered, mut file_errors) = discover_metric_files(&self.dir)?;
        let mut seen = HashSet::new();

        for params in discovered {
            seen.insert(params.path.clone());
            let path = params.path.clone();
            let result: Result<()> = (|| match self.files.get_mut(&path) {
                Some(cached) => {
                    cached.params = params.clone();
                    let metadata = std::fs::metadata(&path)?;
                    if cached.identity != file_identity(&metadata) {
                        *cached = CachedFile::open(params)?;
                    } else {
                        cached.refresh_map_if_needed()?;
                    }
                    Ok(())
                }
                None => {
                    self.files.insert(path.clone(), CachedFile::open(params)?);
                    Ok(())
                }
            })();
            if let Err(err) = result {
                record_file_error(&path, &err);
                file_errors += 1;
                self.files.remove(&path);
            }
        }

        self.files.retain(|path, _| seen.contains(path));
        Ok(file_errors)
    }
}

impl CachedFile {
    fn open(params: FileParams) -> Result<Self> {
        let file = OpenOptions::new().read(true).open(&params.path)?;
        let metadata = file.metadata()?;
        let map = map_file(&file, metadata.len() as usize)?;
        let identity = file_identity(&metadata);
        let mut this = Self {
            params,
            file,
            map,
            identity,
            parsed_used: HEADER_SIZE,
            entries: Vec::new(),
            active: false,
        };
        this.refresh_entries()?;
        Ok(this)
    }

    fn refresh_map_if_needed(&mut self) -> Result<()> {
        let metadata = self.file.metadata()?;
        let len = metadata.len() as usize;
        let identity = file_identity(&metadata);
        if len != self.map.len() || identity != self.identity {
            self.map = map_file(&self.file, len)?;
            self.identity = identity;
            self.parsed_used = HEADER_SIZE;
            self.entries.clear();
        }
        Ok(())
    }

    fn refresh_entries(&mut self) -> Result<()> {
        self.refresh_map_if_needed()?;
        if self.map.len() < HEADER_SIZE {
            self.active = false;
            self.entries.clear();
            self.parsed_used = HEADER_SIZE;
            return Ok(());
        }

        let version = u32::from_ne_bytes(self.map[4..8].try_into().expect("slice len checked"));
        if version != FILE_FORMAT_VERSION {
            self.active = false;
            self.entries.clear();
            self.parsed_used = HEADER_SIZE;
            return Ok(());
        }
        self.active = true;

        let used = self.current_used()?;
        if used < self.parsed_used {
            self.entries.clear();
            self.parsed_used = HEADER_SIZE;
        }

        let mut pos = self.parsed_used.max(HEADER_SIZE);
        while pos + 12 <= used {
            let entry = match RawEntry::from_slice(&self.map[pos..used]) {
                Ok(entry) => entry,
                Err(_) => break,
            };
            let metric = StoredMetricKey::new(
                std::str::from_utf8(entry.family_bytes())?,
                std::str::from_utf8(entry.sample_bytes())?,
                std::str::from_utf8(entry.labels_bytes())?,
            );
            let value_offset = checked_add(
                pos,
                RawEntry::calc_value_offset(
                    entry.family_bytes().len(),
                    entry.sample_bytes().len(),
                    entry.labels_bytes().len(),
                )?,
            )?;
            self.entries.push(CachedEntry {
                metric,
                value_offset,
            });
            pos = checked_add(pos, entry.total_len())?;
        }
        self.parsed_used = pos;
        Ok(())
    }

    fn current_used(&self) -> Result<usize> {
        if self.map.len() < HEADER_SIZE {
            return Ok(HEADER_SIZE);
        }
        let used =
            u32::from_ne_bytes(self.map[0..4].try_into().expect("slice len checked")) as usize;
        let used = if used == 0 { HEADER_SIZE } else { used };
        if used > self.map.len() {
            return Err(MmapError::InvalidUsed {
                used,
                len: self.map.len(),
            });
        }
        Ok(used)
    }

    fn read_value(&self, offset: usize) -> Result<f64> {
        if offset + 8 > self.map.len() {
            return Err(MmapError::OutOfBounds {
                offset: offset + 8,
                len: self.map.len(),
            });
        }
        let bytes: [u8; 8] = self.map[offset..offset + 8]
            .try_into()
            .expect("slice len checked");
        Ok(f64::from_ne_bytes(bytes))
    }
}

fn map_file(file: &File, len: usize) -> Result<Mmap> {
    if len == 0 {
        let empty = unsafe { MmapOptions::new().len(0).map(file)? };
        return Ok(empty);
    }
    Ok(unsafe { MmapOptions::new().len(len).map(file)? })
}

fn discover_metric_files(dir: &Path) -> Result<(Vec<FileParams>, usize)> {
    let mut files = Vec::new();
    let mut file_errors = 0;
    for entry in read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("prometheus-mmap-exporter: unable to read directory entry: {err}");
                file_errors += 1;
                continue;
            }
        };
        let path = entry.path();
        if path.extension() != Some(OsStr::new("db")) {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                record_file_error(&path, &err);
                file_errors += 1;
                continue;
            }
        };
        if !file_type.is_file() {
            continue;
        }
        match parse_file_params(&path) {
            Ok(params) => files.push(params),
            Err(err) => {
                record_file_error(&path, &err);
                file_errors += 1;
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((files, file_errors))
}

fn record_file_error(path: &Path, err: impl std::fmt::Display) {
    eprintln!(
        "prometheus-mmap-exporter: skipping '{}': {err}",
        path.display()
    );
}

fn parse_file_params(path: &Path) -> Result<FileParams> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| MmapError::Message(format!("invalid filename '{}'", path.display())))?;

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

fn render(merged: HashMap<EntryKey, EntryMeta>, file_errors: usize) -> Result<String> {
    let mut rows: Vec<(EntryKey, EntryMeta)> = merged.into_iter().collect();
    rows.sort_by(|a, b| compare_keys(&a.0, &b.0));

    let mut out = String::new();
    let mut previous_family: Option<String> = None;

    for (key, meta) in rows {
        if previous_family.as_deref() != Some(key.metric.family.as_str()) {
            out.push_str("# HELP ");
            out.push_str(&key.metric.family);
            out.push_str(" Multiprocess metric\n");
            out.push_str("# TYPE ");
            out.push_str(&key.metric.family);
            out.push(' ');
            out.push_str(meta.metric_type.as_prometheus_type());
            out.push('\n');
            previous_family = Some(key.metric.family.clone());
        }

        append_sample(
            &mut out,
            &key.metric.sample,
            &key.metric.labels,
            key.pid.as_deref(),
        );
        out.push(' ');
        out.push_str(&meta.value.to_string());
        out.push('\n');
    }

    out.push_str("# HELP prometheus_mmap_exporter_file_errors Number of metric files skipped during this scrape\n");
    out.push_str("# TYPE prometheus_mmap_exporter_file_errors gauge\n");
    out.push_str("prometheus_mmap_exporter_file_errors ");
    out.push_str(&file_errors.to_string());
    out.push('\n');

    Ok(out)
}

fn append_sample(out: &mut String, sample: &str, labels: &str, pid: Option<&str>) {
    out.push_str(sample);
    let Some(pid) = pid else {
        out.push_str(labels);
        return;
    };

    let pid = escape_label_value(pid);
    if let Some(head) = labels.strip_suffix('}') {
        out.push_str(head);
        if head.contains('{') {
            out.push_str(",pid=\"");
        } else {
            out.push_str("{pid=\"");
        }
        out.push_str(&pid);
        out.push_str("\"}");
        return;
    }

    out.push_str(labels);
    out.push_str("{pid=\"");
    out.push_str(&pid);
    out.push_str("\"}");
}

fn compare_keys(a: &EntryKey, b: &EntryKey) -> Ordering {
    match a.metric.family.cmp(&b.metric.family) {
        Ordering::Equal => match a.metric.sample.cmp(&b.metric.sample) {
            Ordering::Equal => match a.metric.labels.cmp(&b.metric.labels) {
                Ordering::Equal => a.pid.cmp(&b.pid),
                ord => ord,
            },
            ord => ord,
        },
        ord => ord,
    }
}

fn file_identity(meta: &Metadata) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Some(FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        });
    }

    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

#[cfg(test)]
mod test {
    use super::CachedDirExporter;
    use crate::mmap_file::{FILE_FORMAT_VERSION, MmapMetricStore};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn exporter_renders_current_values() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("counter_123-0.db");
        let mut store = MmapMetricStore::open(&path).unwrap();
        store
            .increment(
                "http_requests_total",
                "http_requests_total",
                r#"{route="/"}"#,
                1.0,
            )
            .unwrap();
        store.flush().unwrap();

        let mut exporter = CachedDirExporter::new(dir.path()).unwrap();
        let rendered = exporter.render().unwrap();
        assert!(rendered.contains("http_requests_total{route=\"/\"} 1"));

        store
            .increment(
                "http_requests_total",
                "http_requests_total",
                r#"{route="/"}"#,
                2.0,
            )
            .unwrap();
        store
            .increment(
                "http_requests_total",
                "http_requests_total",
                r#"{route="/hello"}"#,
                1.0,
            )
            .unwrap();
        store.flush().unwrap();

        let rendered = exporter.render().unwrap();
        assert!(rendered.contains("http_requests_total{route=\"/\"} 3"));
        assert!(rendered.contains("http_requests_total{route=\"/hello\"} 1"));
    }

    #[test]
    fn exporter_skips_corrupt_file_and_reports_it() {
        let dir = tempdir().unwrap();
        let healthy_path = dir.path().join("counter_123-0.db");
        let mut store = MmapMetricStore::open(&healthy_path).unwrap();
        store
            .increment("requests_total", "requests_total", "", 1.0)
            .unwrap();
        store.flush().unwrap();

        let corrupt_path = dir.path().join("counter_456-0.db");
        let mut corrupt = Vec::new();
        corrupt.extend_from_slice(&1024_u32.to_ne_bytes());
        corrupt.extend_from_slice(&FILE_FORMAT_VERSION.to_ne_bytes());
        fs::write(corrupt_path, corrupt).unwrap();

        let mut exporter = CachedDirExporter::new(dir.path()).unwrap();
        let rendered = exporter.render().unwrap();

        assert!(rendered.contains("requests_total 1"));
        assert!(rendered.contains("prometheus_mmap_exporter_file_errors 1"));
    }
}
