//! The row layer: tables and indexes mapped onto keys (ADR-0007).
//!
//! Functions take a [`TableDef`](crate::catalog::TableDef) read from the
//! catalog in the same transaction. Writes maintain every index in the same
//! write set and enforce primary key, unique and NOT NULL constraints.

mod batch;
mod keys;
mod read;
mod write;

pub use batch::{delete_many, insert_many, update_many};
pub(crate) use keys::{
    index_entry as index_entry_of, index_range, pk_from_index_entry, pk_key, table_range,
    unique_key,
};
pub(crate) use read::read_range_latest;
pub use read::{
    IndexBatch, RowBatch, RowScan, ValueRange, check_indexes, get, get_many,
    register_row_dependency, register_scan_dependency, scan, scan_filtered, scan_index,
    scan_index_many, scan_many,
};
pub(crate) use write::read_latest;
pub use write::{Row, delete, insert, update};
