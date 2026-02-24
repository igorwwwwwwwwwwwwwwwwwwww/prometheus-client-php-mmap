#![cfg_attr(windows, feature(abi_vectorcall))]

pub mod aggregate;
pub mod error;
pub mod mmap_file;
pub mod php_ext;
pub mod raw_entry;

pub use aggregate::aggregate_dir_to_prometheus_text;
pub use mmap_file::MmapMetricStore;
