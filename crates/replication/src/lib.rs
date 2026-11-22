//! Replication of Interlaken storage with Raft (ADR-0037).
//!
//! A replicated database is a set of [`ReplicaNode`]s, each with its own
//! copy of the storage, kept identical by one Raft group (openraft). The
//! leader validates commits and proposes them as log entries; every
//! replica applies committed entries in order. Transactions can begin on
//! any replica and read its applied state; commits go through the leader.
//!
//! Replicas talk through a [`Transport`]. [`MemoryNetwork`] connects
//! replicas in one process, for tests.

pub mod commit;
pub mod log_store;
pub mod network;
mod node;
mod read;
pub mod state_machine;
pub mod transport;
pub mod types;

pub use commit::{CommitConfig, RaftCommit};
pub use node::{ClusterStatus, ReplicaConfig, ReplicaNode};
pub use transport::{
    Handler, MembershipChange, MemoryNetwork, MemoryTransport, Request, Response, Transport,
    TransportError,
};
