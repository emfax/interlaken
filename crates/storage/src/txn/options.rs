//! Transaction options.

/// Isolation level of a transaction (ADR-0004).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Isolation {
    /// One snapshot per statement, refreshed by [`Txn::next_statement`](crate::Txn::next_statement).
    ReadCommitted,
    /// Snapshot isolation: one snapshot for the whole transaction.
    RepeatableRead,
    /// Serializable: snapshot isolation plus read-set validation at commit.
    #[default]
    Serializable,
}

/// Options for [`Storage::begin`](crate::Storage::begin).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TxnOptions {
    /// Isolation level. Defaults to [`Isolation::Serializable`].
    pub isolation: Isolation,
    /// Reject writes. Read-only transactions skip commit validation.
    pub read_only: bool,
}

impl TxnOptions {
    /// Construct read-write options at the given isolation level.
    #[must_use]
    pub const fn new(isolation: Isolation) -> Self {
        Self {
            isolation,
            read_only: false,
        }
    }

    /// Return these options marked read-only.
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}
