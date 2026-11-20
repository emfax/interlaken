//! The fjall database, its keyspaces and MVCC reads (ADR-0001, ADR-0002).

mod database;
pub(crate) mod gc;
mod meta;
pub(crate) mod mvcc;
mod stored;

pub use database::Engine;
pub(crate) use database::{DEFAULT_MAX_JOURNAL_BYTES, DEFAULT_MEMTABLE_BYTES};
pub use meta::FORMAT_VERSION;
