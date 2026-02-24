use crate::aggregate::aggregate_dir_to_prometheus_text;
use crate::mmap_file::MmapMetricStore;
use ext_php_rs::prelude::*;

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
    pub fn increment(&self, key_json: String, by: f64) -> PhpResult<f64> {
        let mut store = MmapMetricStore::open(&self.path).map_err(to_php_exception)?;
        store.increment(&key_json, by).map_err(to_php_exception)
    }

    pub fn set(&self, key_json: String, value: f64) -> PhpResult<f64> {
        let mut store = MmapMetricStore::open(&self.path).map_err(to_php_exception)?;
        store.set(&key_json, value).map_err(to_php_exception)
    }

    pub fn get(&self, key_json: String) -> PhpResult<f64> {
        let mut store = MmapMetricStore::open(&self.path).map_err(to_php_exception)?;
        store.get(&key_json).map_err(to_php_exception)
    }

    pub fn flush(&self) -> PhpResult<()> {
        let mut store = MmapMetricStore::open(&self.path).map_err(to_php_exception)?;
        store.flush().map_err(to_php_exception)
    }
}

#[php_function]
pub fn prometheus_mmap_render_dir(dir: String) -> PhpResult<String> {
    aggregate_dir_to_prometheus_text(&dir).map_err(to_php_exception)
}

#[php_module]
pub fn get_module(module: ModuleBuilder) -> ModuleBuilder {
    module
        .class::<PhpMmapStore>()
        .function(wrap_function!(prometheus_mmap_render_dir))
}

fn to_php_exception(err: impl ToString) -> ext_php_rs::exception::PhpException {
    err.to_string().into()
}
