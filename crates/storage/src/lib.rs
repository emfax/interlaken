//! Transactional MVCC key-value storage layer on fjall.
//!
//! This crate is the bottom of the interlaken stack. It currently provides:
//!
//! - [`encoding`]: the order-preserving tuple encoding used for every key
//!   (ADR-0003) and the versioned key layout (ADR-0002).
//! - [`Engine`]: the fjall database with its `data` and `meta` keyspaces
//!   (ADR-0001, ADR-0002).
//! - [`Timestamp`]: the opaque commit timestamp carried in versioned keys.
//! - [`Storage`] and [`FjallStorage`]: the batched async interface (ADR-0006).
//!   All fjall calls run on a dedicated storage pool; async code never calls
//!   fjall directly (ADR-0001).
//! - [`Txn`]: a cloneable transaction handle with MVCC snapshot reads, a
//!   statement-tagged write buffer, a read set and savepoints (ADR-0004).
//! - The commit sequencer: optimistic validation for READ COMMITTED,
//!   REPEATABLE READ and SERIALIZABLE, group commit, and the
//!   [`Durability`] modes (ADR-0004, ADR-0005).

mod commit;
pub mod encoding;
mod engine;
mod error;
mod pool;
mod scan;
mod storage;
mod timestamp;
mod txn;

pub use commit::CommitStats;
pub use engine::{Engine, FORMAT_VERSION};
pub use error::{Conflict, Error, Result};
pub use scan::{
    DEFAULT_SCAN_LIMIT_BYTES, DEFAULT_SCAN_LIMIT_ROWS, Key, KeyRange, RowFilter, ScanBatch,
    ScanRequest,
};
pub use storage::{Durability, FjallStorage, Storage, StorageOptions};
pub use timestamp::Timestamp;
pub use txn::{CommitInfo, Isolation, SavepointId, Txn, TxnOptions};
