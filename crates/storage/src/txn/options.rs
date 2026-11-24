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

impl Isolation {
    /// Return the level's metric label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ReadCommitted => "read_committed",
            Self::RepeatableRead => "repeatable_read",
            Self::Serializable => "serializable",
        }
    }
}

/// How recent a transaction's snapshot must be on a replicated database
/// (ADR-0037). A single-node database is always fresh.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Freshness {
    /// See every transaction acknowledged anywhere in the cluster before
    /// this one began: a linearizable snapshot. On a follower it costs a
    /// round trip to the leader, shared by concurrent transactions.
    #[default]
    Fresh,
    /// Read this replica's applied state, which may lag the leader. Applies
    /// to read-only transactions; read-write ones are always fresh.
    Local,
}

/// Options for [`Storage::begin`](crate::Storage::begin).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TxnOptions {
    /// Isolation level. Defaults to [`Isolation::Serializable`].
    pub isolation: Isolation,
    /// Reject writes. Read-only transactions skip commit validation.
    pub read_only: bool,
    /// How recent the snapshot must be. Defaults to [`Freshness::Fresh`].
    pub freshness: Freshness,
}

impl TxnOptions {
    /// Construct read-write options at the given isolation level.
    #[must_use]
    pub const fn new(isolation: Isolation) -> Self {
        Self {
            isolation,
            read_only: false,
            freshness: Freshness::Fresh,
        }
    }

    /// Return these options with the given freshness.
    #[must_use]
    pub const fn freshness(mut self, freshness: Freshness) -> Self {
        self.freshness = freshness;
        self
    }

    /// Return these options marked read-only.
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}
