//! Crate-level error type.

use std::fmt;
use std::sync::Arc;

use crate::encoding::{DecodeError, EncodeError, RowFormatError};

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

    /// A replicated database can't commit now: it has no leader, or the
    /// leader can't be reached (SQLSTATE `57P03`). The transaction didn't
    /// commit; retry it (ADR-0037).
    #[error("the cluster can't commit now: {0}")]
    ReplicationUnavailable(String),

    /// The commit sequencer has stopped after a failed write or a panic.
    #[error("the commit sequencer has stopped")]
    SequencerStopped,

    /// The savepoint was released, rolled back past, or belongs to another transaction.
    #[error("unknown savepoint")]
    UnknownSavepoint,

    /// The transaction has been open longer than the maximum age (SQLSTATE
    /// `25P04`). Versions it needs may have been garbage-collected, so it
    /// can't continue; begin a new transaction.
    #[error("transaction exceeded the maximum age of {max_age:?}")]
    TransactionTooOld {
        /// The configured maximum age.
        max_age: std::time::Duration,
    },

    /// The transaction's buffered writes exceed the limit (SQLSTATE `54000`).
    #[error("transaction write set exceeds {limit} bytes")]
    WriteSetTooLarge {
        /// The configured limit in bytes.
        limit: usize,
    },

    /// A row value could not be encoded, or a stored row could not be decoded.
    #[error(transparent)]
    Row(#[from] RowFormatError),

    /// A DDL request or catalog lookup failed.
    #[error(transparent)]
    Catalog(#[from] CatalogError),

    /// A row violates a table constraint.
    #[error(transparent)]
    Constraint(#[from] ConstraintViolation),
}

/// A DDL or catalog error (ADR-0007).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    /// A table with this name already exists (SQLSTATE `42P07`).
    #[error("relation \"{0}\" already exists")]
    TableExists(String),
    /// No table has this name (SQLSTATE `42P01`).
    #[error("relation \"{0}\" does not exist")]
    NoSuchTable(String),
    /// An index with this name already exists (SQLSTATE `42P07`).
    #[error("index \"{0}\" already exists")]
    IndexExists(String),
    /// No index has this name (SQLSTATE `42704`).
    #[error("index \"{0}\" does not exist")]
    NoSuchIndex(String),
    /// The table already has a column with this name (SQLSTATE `42701`).
    #[error("column \"{column}\" of relation \"{table}\" already exists")]
    ColumnExists {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
    },
    /// The table has no column with this name (SQLSTATE `42703`).
    #[error("column \"{column}\" of relation \"{table}\" does not exist")]
    NoSuchColumn {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
    },
    /// The definition is not allowed, for example a table without columns.
    #[error("invalid definition: {0}")]
    InvalidDefinition(String),
    /// A system table holds a malformed entry.
    #[error("corrupt catalog: {0}")]
    Corrupt(&'static str),
}

/// A constraint a row violates (ADR-0007).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConstraintViolation {
    /// Another row has the same primary key (SQLSTATE `23505`).
    #[error("duplicate key value violates the primary key of \"{table}\"")]
    PrimaryKey {
        /// Table name.
        table: String,
    },
    /// Another row has the same values in a unique index (SQLSTATE `23505`).
    #[error("duplicate key value violates unique constraint \"{index}\"")]
    Unique {
        /// Index name.
        index: String,
    },
    /// A NOT NULL column was given NULL (SQLSTATE `23502`).
    #[error(
        "null value in column \"{column}\" of relation \"{table}\" violates not-null constraint"
    )]
    NotNull {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
    },
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

    /// Return the Postgres SQLSTATE code for this error.
    ///
    /// Catalog errors are `XX000` here; the engine maps them to their own
    /// codes, since it knows which object a lookup was for.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_storage::{Conflict, Error};
    ///
    /// assert_eq!(Error::SerializationFailure(Conflict::WriteWrite).sqlstate(), "40001");
    /// assert_eq!(Error::ReadOnlyTransaction.sqlstate(), "25006");
    /// ```
    #[must_use]
    pub fn sqlstate(&self) -> &'static str {
        match self {
            Self::SerializationFailure(_) => "40001",
            Self::Constraint(ConstraintViolation::NotNull { .. }) => "23502",
            Self::Constraint(_) => "23505",
            Self::ReadOnlyTransaction => "25006",
            Self::TransactionTooOld { .. } => "25P04",
            Self::WriteSetTooLarge { .. } => "54000",
            Self::CommitOutcomeUnknown(_) => "08007",
            Self::ReplicationUnavailable(_) => "57P03",
            _ => "XX000",
        }
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
    /// A transaction that committed after this one's snapshot wrote a key,
    /// or a key inside a range, that this one registered as a dependency
    /// (checked at every isolation level). The usual cause is concurrent DDL
    /// on a table this transaction wrote to.
    DependencyChanged,
    /// The snapshot is older than the commit history kept for validation.
    SnapshotTooOld,
}

impl fmt::Display for Conflict {
    /// Render the conflict as a short reason.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WriteWrite => "concurrent update to a key this transaction writes",
            Self::ReadWrite => "concurrent update to data this transaction read",
            Self::DependencyChanged => "concurrent change to a definition this transaction used",
            Self::SnapshotTooOld => "snapshot is older than the retained commit history",
        })
    }
}
