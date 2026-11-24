//! How replicas talk to each other (ADR-0037).
//!
//! The core depends only on [`Transport`]: send one [`Request`] to a node
//! and get its [`Response`]. Requests are Raft's own messages and
//! forwarded commits. Each node answers requests through
//! the [`Handler`] that [`ReplicaNode::handler`](crate::ReplicaNode::handler) returns.
//!
//! Errors say whether the request surely didn't arrive
//! ([`TransportError::NotDelivered`]) or might have
//! ([`TransportError::NoReply`]). The difference matters for forwarded
//! commits: one that might have arrived might have committed, so it is
//! never sent again.
//!
//! [`MemoryNetwork`] connects nodes in one process, with partitions, for
//! tests. A turmoil transport tests under a simulated network, and phase 5
//! adds gRPC.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use interlaken_storage::Conflict;
use openraft::alias::{SnapshotMetaOf, VoteOf};
use openraft::raft::{
    AppendEntriesRequest, SnapshotResponse, StreamAppendResult, TransferLeaderRequest,
    TransferLeaderResponse, VoteRequest, VoteResponse,
};
use serde::{Deserialize, Serialize};

use crate::types::{NodeId, TypeConfig, decode, encode};

/// A message one replica sends another.
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// Raft's AppendEntries.
    Append(AppendEntriesRequest<TypeConfig>),
    /// Raft's RequestVote.
    Vote(VoteRequest<TypeConfig>),
    /// Raft's pre-vote.
    PreVote(VoteRequest<TypeConfig>),
    /// A commit request, encoded, forwarded to the leader.
    Commit(Vec<u8>),
    /// A read index, for a fresh snapshot on the sender.
    ReadIndex,
    /// One chunk of a snapshot from the leader, for a replica too far
    /// behind its log (ADR-0038). The receiver stores chunks in order, and
    /// installs the snapshot once the last arrives.
    SnapshotChunk {
        /// The transfer the chunk belongs to.
        id: u64,
        /// The chunk's place in the transfer, from 0.
        seq: u64,
        /// The chunk.
        bytes: Vec<u8>,
        /// On the last chunk, what installing needs.
        end: Option<SnapshotEnd>,
    },
    /// Raft's request that the receiver take over leadership.
    TransferLeader(TransferLeaderRequest<TypeConfig>),
    /// A membership change, forwarded to the leader.
    Membership(MembershipChange),
}

impl Request {
    /// Return the request's kind, for metrics.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Append(_) => "append",
            Self::Vote(_) => "vote",
            Self::PreVote(_) => "pre_vote",
            Self::Commit(_) => "commit",
            Self::ReadIndex => "read_index",
            Self::SnapshotChunk { .. } => "snapshot_chunk",
            Self::TransferLeader(_) => "transfer_leader",
            Self::Membership(_) => "membership",
        }
    }
}

/// What a snapshot transfer's last chunk carries.
#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotEnd {
    /// The sending leader's vote.
    pub vote: VoteOf<TypeConfig>,
    /// The snapshot's Raft metadata.
    pub meta: SnapshotMetaOf<TypeConfig>,
}

/// A change to the cluster's membership (ADR-0037 phase 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MembershipChange {
    /// Add a non-voting replica, which catches up from the leader.
    AddLearner {
        /// The new replica.
        id: NodeId,
        /// Its address, as transports resolve it.
        addr: String,
    },
    /// Make exactly these nodes the voters, through joint consensus.
    /// Voters left out become learners if `retain`, else leave.
    SetVoters {
        /// The new voters.
        voters: Vec<NodeId>,
        /// Whether removed voters stay as learners.
        retain: bool,
    },
    /// Remove learners.
    RemoveLearners(Vec<NodeId>),
    /// Hand leadership to a voter.
    TransferLeadership(NodeId),
}

/// A replica's answer to a [`Request`].
#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    /// The answer to [`Request::Append`], without openraft's conflict hint,
    /// which has no serialized form.
    Append(StreamAppendResult<TypeConfig>),
    /// The answer to [`Request::Vote`] or [`Request::PreVote`].
    Vote(VoteResponse<TypeConfig>),
    /// The answer to [`Request::Commit`].
    Commit(Result<Committed, CommitError>),
    /// The answer to [`Request::ReadIndex`]: the log index the sender must
    /// apply before its snapshot is fresh.
    ReadIndex(Result<u64, CommitError>),
    /// The answer to a snapshot chunk other than the last: stored, or why not.
    SnapshotChunk(Result<(), String>),
    /// The answer to a snapshot transfer's last chunk.
    Snapshot(SnapshotResponse<TypeConfig>),
    /// The answer to [`Request::TransferLeader`].
    TransferLeader(TransferLeaderResponse<TypeConfig>),
    /// The answer to [`Request::Membership`].
    Membership(Result<(), CommitError>),
    /// The node couldn't handle the request, such as before Raft started.
    Refused(String),
}

/// Where a forwarded commit went in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Committed {
    /// The index of the log entry holding the commit.
    pub index: u64,
    /// The commit timestamp.
    pub ts: u64,
}

/// Why a forwarded commit didn't commit, or might not have.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitError {
    /// The receiver isn't the leader; the commit didn't happen.
    NotLeader,
    /// The commit didn't happen and retrying soon won't help.
    Unavailable(String),
    /// Validation failed, with the conflict's code ([`conflict_code`]).
    Conflict(u8),
    /// The commit timestamp space is exhausted.
    TimestampExhausted,
    /// The commit may or may not have happened.
    Unknown(String),
}

/// Return the wire code of a conflict.
#[must_use]
pub fn conflict_code(conflict: Conflict) -> u8 {
    match conflict {
        Conflict::WriteWrite => 0,
        Conflict::ReadWrite => 1,
        Conflict::DependencyChanged => 2,
        Conflict::SnapshotTooOld => 3,
        _ => 4,
    }
}

/// Return the conflict a wire code stands for.
#[must_use]
pub fn conflict_of(code: u8) -> Conflict {
    match code {
        0 => Conflict::WriteWrite,
        1 => Conflict::ReadWrite,
        2 => Conflict::DependencyChanged,
        3 => Conflict::SnapshotTooOld,
        _ => Conflict::ReadWrite,
    }
}

/// Why a request got no answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The request surely didn't reach the node.
    #[error("not delivered: {0}")]
    NotDelivered(String),
    /// The request may have reached the node, but no answer came back.
    #[error("no reply: {0}")]
    NoReply(String),
}

/// Sends requests to other replicas.
pub trait Transport: Send + Sync + 'static {
    /// Send `request` to node `target` and return its response.
    ///
    /// # Errors
    ///
    /// A [`TransportError`] saying whether the request may have arrived.
    fn send(
        &self,
        target: NodeId,
        request: Request,
    ) -> impl Future<Output = Result<Response, TransportError>> + Send;
}

/// The future a [`Handler`] returns.
pub type HandlerFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

/// Answers requests: a replica node, as a network sees it.
pub trait Handler: Send + Sync + 'static {
    /// Answer `request`.
    fn handle(&self, request: Request) -> HandlerFuture;
}

/// Lock a mutex, ignoring poisoning.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Replicas in one process, connected directly, with partitions for tests.
///
/// Every request and response is encoded and decoded on the way, as a
/// real network would, so serialization is tested too.
#[derive(Default)]
pub struct MemoryNetwork {
    /// The registered nodes.
    nodes: Mutex<HashMap<NodeId, Arc<dyn Handler>>>,
    /// Pairs of nodes that can't reach each other, in either direction.
    cut: Mutex<BTreeSet<(NodeId, NodeId)>>,
}

impl std::fmt::Debug for MemoryNetwork {
    /// Render the network's cut links.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryNetwork")
            .field("cut", &*lock(&self.cut))
            .finish_non_exhaustive()
    }
}

impl MemoryNetwork {
    /// Construct an empty network.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Return a transport for node `from`.
    #[must_use]
    pub fn transport(self: &Arc<Self>, from: NodeId) -> MemoryTransport {
        MemoryTransport {
            network: Arc::clone(self),
            from,
        }
    }

    /// Register `handler` as node `id`, replacing any earlier one.
    pub fn register(&self, id: NodeId, handler: Arc<dyn Handler>) {
        lock(&self.nodes).insert(id, handler);
    }

    /// Remove node `id`, as if it crashed.
    pub fn unregister(&self, id: NodeId) {
        lock(&self.nodes).remove(&id);
    }

    /// Cut the link between `a` and `b`.
    pub fn cut(&self, a: NodeId, b: NodeId) {
        lock(&self.cut).insert((a.min(b), a.max(b)));
    }

    /// Cut every link between `group` and the nodes outside it among `all`.
    pub fn partition(&self, group: &[NodeId], all: &[NodeId]) {
        for &a in group {
            for &b in all.iter().filter(|b| !group.contains(b)) {
                self.cut(a, b);
            }
        }
    }

    /// Restore every link.
    pub fn heal(&self) {
        lock(&self.cut).clear();
    }

    /// Return `true` if `a` and `b` can't reach each other.
    fn is_cut(&self, a: NodeId, b: NodeId) -> bool {
        lock(&self.cut).contains(&(a.min(b), a.max(b)))
    }
}

/// One node's view of a [`MemoryNetwork`].
#[derive(Clone, Debug)]
pub struct MemoryTransport {
    /// The network.
    network: Arc<MemoryNetwork>,
    /// The sending node.
    from: NodeId,
}

impl Transport for MemoryTransport {
    /// Deliver `request` to `target` unless the link is cut or the node is
    /// gone, and return its response unless the link was cut meanwhile.
    async fn send(&self, target: NodeId, request: Request) -> Result<Response, TransportError> {
        if self.network.is_cut(self.from, target) {
            return Err(TransportError::NotDelivered(format!(
                "link {}-{target} is cut",
                self.from
            )));
        }
        let handler = lock(&self.network.nodes)
            .get(&target)
            .cloned()
            .ok_or_else(|| TransportError::NotDelivered(format!("node {target} is down")))?;
        let wire = |e: std::io::Error| TransportError::NotDelivered(e.to_string());
        let request: Request = decode(&encode(&request).map_err(wire)?).map_err(wire)?;
        let response = handler.handle(request).await;
        if self.network.is_cut(self.from, target) {
            return Err(TransportError::NoReply(format!(
                "link {}-{target} was cut",
                self.from
            )));
        }
        let lost = |e: std::io::Error| TransportError::NoReply(e.to_string());
        decode(&encode(&response).map_err(lost)?).map_err(lost)
    }
}
