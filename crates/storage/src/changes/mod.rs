//! The change log: each committed transaction's row writes, by commit
//! timestamp, kept so a change feed can follow and resume (ADR-0040).

mod entry;
mod log;
mod retention;

pub use entry::{ChangeEntry, ChangeWrite};
pub(crate) use entry::{decode_durable_key, durable_key, entries_from};
pub(crate) use log::ChangeLog;
pub use retention::ChangeRetention;
