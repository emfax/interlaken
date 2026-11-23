//! The fjall database, its keyspaces and MVCC reads (ADR-0001, ADR-0002).

mod database;
pub(crate) mod gc;
mod meta;
pub(crate) mod mvcc;
pub(crate) mod snapshot;
mod stored;

pub use database::Engine;
pub(crate) use database::{DEFAULT_MAX_JOURNAL_BYTES, DEFAULT_MEMTABLE_BYTES};
pub(crate) use database::{Staged, Staging};
pub use meta::FORMAT_VERSION;
pub use snapshot::ExportCursor;
