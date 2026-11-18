//! Crate-level error type.

use std::fmt;
use std::sync::Arc;

use crate::encoding::{DecodeError, EncodeError};

/// A specialized [`Result`](std::result::Result) for storage operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors returned by the storage layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The underlying fjall engine reported an error.
    #[error("storage engine error: {0}")]
    Fjall(#[from] fjall::Error),

    /// The database directory was written by an incompatible build.
    #[error("unsupported on-disk format version {found} (this build supports {supported})")]
    UnsupportedFormatVersion {
        /// Version found in the `meta` keyspace.
        found: u32,
        /// Version this build reads and writes.
        supported: u32,
    },

    /// The `meta` keyspace is missing an entry or holds a malformed value.
    #[error("corrupt engine metadata: {0}")]
    CorruptMeta(&'static str),

    /// A key could not be encoded.
    #[error(transparent)]
    Encode(#[from] EncodeError),

    /// A key could not be decoded.
    #[error(transparent)]
    Decode(#[from] DecodeError),

    /// A value in the `data` keyspace has an unknown record tag.
    #[error("corrupt stored value: unknown record tag {0:#04x}")]
    CorruptValue(u8),

    /// The storage pool has shut down, or a storage task panicked.
    #[error("storage pool is unavailable: {0}")]
    PoolUnavailable(&'static str),

    /// A write was attempted in a read-only transaction.
    #[error("cannot write in a read-only transaction")]
    ReadOnlyTransaction,

    /// `commit` was called while other clones of the transaction handle are alive.
    #[error("cannot commit while other clones of the transaction handle are alive")]
    TransactionShared,

    /// The logical commit timestamp space is exhausted.
    #[error("commit timestamp space exhausted")]
    TimestampExhausted,

    /// Commit validation failed (SQLSTATE `40001`); retry the transaction.
    #[error("could not serialize access: {0}")]
    SerializationFailure(Conflict),

    /// Writing a commit group failed, so the commit may or may not be durable.
    ///
    /// The commit sequencer stops after this error and every later commit
    /// fails with [`Error::SequencerStopped`]; reopen the database to recover.
    #[error("commit outcome unknown: {0}")]
    CommitOutcomeUnknown(#[source] Arc<Error>),

    /// The commit sequencer has stopped after a failed write or a panic.
    #[error("the commit sequencer has stopped")]
    SequencerStopped,

    /// The savepoint was released, rolled back past, or belongs to another transaction.
    #[error("unknown savepoint")]
    UnknownSavepoint,
}

impl Error {
    /// Return `true` if retrying the whole transaction may succeed.
    ///
    /// Only [`Error::SerializationFailure`] is retryable.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_storage::{Conflict, Error};
    ///
    /// assert!(Error::SerializationFailure(Conflict::WriteWrite).is_retryable());
    /// assert!(!Error::SequencerStopped.is_retryable());
    /// ```
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::SerializationFailure(_))
    }
}

/// Why commit validation rejected a transaction (ADR-0004).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Conflict {
    /// A transaction that committed after this one's snapshot wrote a key this one also writes.
    WriteWrite,
    /// A transaction that committed after this one's snapshot wrote a key, or
    /// a key inside a range, that this one read (SERIALIZABLE only).
    ReadWrite,
    /// The snapshot is older than the commit history kept for validation.
    SnapshotTooOld,
}

impl fmt::Display for Conflict {
    /// Render the conflict as a short reason.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WriteWrite => "concurrent update to a key this transaction writes",
            Self::ReadWrite => "concurrent update to data this transaction read",
            Self::SnapshotTooOld => "snapshot is older than the retained commit history",
        })
    }
}
