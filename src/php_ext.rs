use crate::aggregate::aggregate_dir_to_prometheus_text;
use crate::error::MmapError;
use crate::gc::gc_metric_files;
use crate::mmap_file::MmapMetricStore;
use ext_php_rs::prelude::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

static STORE_CACHE: OnceLock<Mutex<HashMap<String, MmapMetricStore>>> = OnceLock::new();

#[php_class]
#[php(name = "PrometheusMmapStore")]
pub struct PhpMmapStore {
    path: String,
}

#[php_impl]
impl PhpMmapStore {
    pub fn __construct(path: String) -> Self {
        Self { path }
    }

    #[php(defaults(by = 1.0))]
    pub fn increment(
        &self,
        family: String,
        sample: String,
        labels: String,
        by: f64,
    ) -> PhpResult<f64> {
        with_cached_store(&self.path, |store| {
            store.increment(&family, &sample, &labels, by)
        })
        .map_err(to_php_exception)
    }

    pub fn set(
        &self,
        family: String,
        sample: String,
        labels: String,
        value: f64,
    ) -> PhpResult<f64> {
        with_cached_store(&self.path, |store| {
            store.set(&family, &sample, &labels, value)
        })
        .map_err(to_php_exception)
    }

    pub fn get(&self, family: String, sample: String, labels: String) -> PhpResult<f64> {
        with_cached_store(&self.path, |store| store.get(&family, &sample, &labels))
            .map_err(to_php_exception)
    }

    pub fn flush(&self) -> PhpResult<()> {
        with_cached_store(&self.path, |store| store.flush()).map_err(to_php_exception)
    }
}

#[php_function]
pub fn prometheus_mmap_render_dir(dir: String) -> PhpResult<String> {
    aggregate_dir_to_prometheus_text(&dir).map_err(to_php_exception)
}

#[php_function]
#[php(defaults(
    budget_ms = 10,
    scan_limit = 64,
    delete_limit = 16,
    dead_grace_sec = 600
))]
pub fn prometheus_mmap_gc_dir(
    dir: String,
    budget_ms: i64,
    scan_limit: i64,
    delete_limit: i64,
    dead_grace_sec: i64,
) -> PhpResult<i64> {
    let budget_ms = budget_ms.max(1) as u64;
    let scan_limit = scan_limit.max(1) as usize;
    let delete_limit = delete_limit.max(1) as usize;
    let dead_grace_sec = dead_grace_sec.max(0) as u64;
    let deleted = gc_metric_files(&dir, budget_ms, scan_limit, delete_limit, dead_grace_sec)
        .map_err(to_php_exception)?;
    Ok(deleted as i64)
}

#[php_module]
pub fn get_module(module: ModuleBuilder) -> ModuleBuilder {
    module
        .class::<PhpMmapStore>()
        .function(wrap_function!(prometheus_mmap_render_dir))
        .function(wrap_function!(prometheus_mmap_gc_dir))
}

fn to_php_exception(err: impl ToString) -> ext_php_rs::exception::PhpException {
    err.to_string().into()
}

fn with_cached_store<T>(
    path: &str,
    f: impl FnOnce(&mut MmapMetricStore) -> Result<T, MmapError>,
) -> Result<T, MmapError> {
    let cache = STORE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache
        .lock()
        .map_err(|_| MmapError::Message("store cache lock poisoned".to_string()))?;

    if !guard.contains_key(path) {
        let store = MmapMetricStore::open(path)?;
        guard.insert(path.to_owned(), store);
    }

    let store = guard
        .get_mut(path)
        .ok_or_else(|| MmapError::Message("store cache lookup failed".to_string()))?;
    f(store)
}
