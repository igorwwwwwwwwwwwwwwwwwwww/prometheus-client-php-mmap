use crate::error::{MmapError, Result, checked_add};
use crate::mmap_file::HEADER_SIZE;
use crate::raw_entry::RawEntry;
use serde::Deserialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, read_dir};
use std::io::Read;
use std::path::{Path, PathBuf};

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
            let entry = RawEntry::from_slice(&bytes[pos..used])?;
            let key = std::str::from_utf8(entry.json())?;
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

            pos = checked_add(pos, entry.total_len())?;
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
    use super::aggregate_dir_to_prometheus_text;
    use crate::mmap_file::MmapMetricStore;
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
}
