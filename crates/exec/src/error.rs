//! Executor errors.

use interlaken_ir::interp::EvalError;

use crate::memory::MemoryScope;

/// Why a query failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExecError {
    /// Evaluating an expression raised an error (overflow, division by zero, ...).
    #[error(transparent)]
    Eval(#[from] EvalError),
    /// A storage operation failed.
    #[error(transparent)]
    Storage(#[from] interlaken_storage::Error),
    /// Building or slicing an Arrow batch failed; indicates an executor bug.
    #[error("arrow: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    /// The plan uses an operator or type this executor milestone does not cover.
    #[error("not supported by the executor yet: {0}")]
    Unsupported(String),
    /// The IR catalog and the stored tables disagree.
    #[error("catalog mismatch: {0}")]
    Catalog(String),
    /// An earlier statement failed, so the transaction accepts nothing but a
    /// rollback (to a savepoint or entirely) (SQLSTATE `25P02`).
    #[error("current transaction is aborted, commands ignored until end of transaction block")]
    TransactionAborted,
    /// A row written to `table` failed its write check (SQLSTATE `42501`).
    /// The message names the table, never a row's values (ADR-0025).
    #[error("permission denied for table {table}")]
    PermissionDenied {
        /// The table written.
        table: String,
    },
    /// An operator needed more memory than the query's budget or the
    /// node's limit allows (SQLSTATE `53200`, ADR-0027).
    #[error("{operator} exceeded the {scope}")]
    MemoryExceeded {
        /// The operator that asked for the memory.
        operator: &'static str,
        /// Which limit it crossed.
        scope: MemoryScope,
    },
    /// The statement ran longer than its `statement_timeout` and was
    /// cancelled; its writes were rolled back (SQLSTATE `57014`).
    #[error("canceling statement due to statement timeout")]
    QueryCanceled,
    /// A membership edge written by DML would make its hierarchy cyclic
    /// (SQLSTATE `23514`, ADR-0017).
    #[error("membership of {member} in {group} would create a cycle")]
    MembershipCycle {
        /// The member entity.
        member: String,
        /// The group entity.
        group: String,
    },
}

impl ExecError {
    /// Return the Postgres SQLSTATE code for this error.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_exec::ExecError;
    ///
    /// assert_eq!(ExecError::TransactionAborted.sqlstate(), "25P02");
    /// ```
    #[must_use]
    pub fn sqlstate(&self) -> &'static str {
        match self {
            Self::Eval(e) => e.sqlstate(),
            Self::Storage(e) => e.sqlstate(),
            Self::Arrow(_) | Self::Catalog(_) => "XX000",
            Self::Unsupported(_) => "0A000",
            Self::TransactionAborted => "25P02",
            Self::PermissionDenied { .. } => "42501",
            Self::MemoryExceeded { .. } => "53200",
            Self::QueryCanceled => "57014",
            Self::MembershipCycle { .. } => "23514",
        }
    }
}
