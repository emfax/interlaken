//! The Raft type configuration and the data the log carries.

use serde::{Deserialize, Serialize};

/// A commit group, as a log entry carries it: the batch a storage
/// [`Proposer`](interlaken_storage::replica::Proposer) made, encoded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch(pub Vec<u8>);

impl std::fmt::Display for Batch {
    /// Render the batch by size, for Raft's logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "batch of {} bytes", self.0.len())
    }
}

openraft::declare_raft_types!(
    /// Interlaken's Raft types: commit groups in, nothing out.
    pub TypeConfig:
        D = Batch,
        R = (),
        NodeId = u64,
        Node = openraft::impls::BasicNode,
);

/// A replica's Raft handle.
pub type Raft = openraft::Raft<TypeConfig, crate::state_machine::StateMachine>;
/// A log entry.
pub type Entry = openraft::alias::EntryOf<TypeConfig>;
/// A log ID: a leader and an index.
pub type LogId = openraft::alias::LogIdOf<TypeConfig>;
/// A vote.
pub type Vote = openraft::alias::VoteOf<TypeConfig>;
/// A membership with the log ID that set it.
pub type StoredMembership = openraft::alias::StoredMembershipOf<TypeConfig>;
/// A node ID.
pub type NodeId = u64;

/// Encode `value` with postcard.
pub(crate) fn encode<T: Serialize>(value: &T) -> std::io::Result<Vec<u8>> {
    postcard::to_stdvec(value).map_err(std::io::Error::other)
}

/// Decode a postcard-encoded value.
pub(crate) fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> std::io::Result<T> {
    postcard::from_bytes(bytes).map_err(std::io::Error::other)
}

/// Convert a storage error to the I/O error openraft's storage traits use.
pub(crate) fn io(error: interlaken_storage::Error) -> std::io::Error {
    std::io::Error::other(error)
}
