//! Shared pieces of the model-based tests: key universe, scans and the reference model.

mod model;
mod scan;

pub use model::{Model, ModelTxn};
pub use scan::{ScanSpec, key, run_scan, scan_spec};
