#![cfg_attr(windows, feature(abi_vectorcall))]

pub mod aggregate;
pub mod error;
pub mod gc;
pub mod metric_key;
pub mod mmap_file;
pub mod php_ext;
pub mod raw_entry;

pub use aggregate::aggregate_dir_to_prometheus_text;
pub use metric_key::{StoredMetricKey, encode_labels};
pub use mmap_file::MmapMetricStore;
