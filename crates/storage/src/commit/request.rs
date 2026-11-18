//! What a transaction sends to the sequencer at commit.

use crate::Timestamp;
use crate::scan::Key;
use crate::txn::{ReadSet, WriteOp};

/// One buffered write, with the snapshot it must be validated against.
#[derive(Clone, Debug)]
pub(crate) struct WriteIntent {
    /// Key written.
    pub(crate) key: Key,
    /// Final write to the key.
    pub(crate) op: WriteOp,
    /// Snapshot of the statement that first wrote the key. A commit after
    /// this timestamp that also wrote the key is a lost update.
    pub(crate) snapshot: Timestamp,
}

/// A transaction ready for validation.
#[derive(Debug)]
pub(crate) struct CommitRequest {
    /// Snapshot the read set was taken at.
    pub(crate) snapshot: Timestamp,
    /// Reads to validate. The transaction leaves this empty unless it is
    /// SERIALIZABLE, so validation needs no isolation level.
    pub(crate) reads: ReadSet,
    /// Writes to validate and apply. Never empty.
    pub(crate) writes: Vec<WriteIntent>,
}
