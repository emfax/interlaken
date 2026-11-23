//! openraft's network, over a [`Transport`].

use std::future::Future;
use std::sync::Arc;

use openraft::alias::{SnapshotOf, VoteOf};
use openraft::error::{NetworkError, RPCError, ReplicationClosed, StreamingError, Unreachable};
use openraft::network::{RPCOption, RaftNetworkFactory, RaftNetworkV2};
use openraft::raft::{
    AppendEntriesRequest, AppendResult, SnapshotResponse, TransferLeaderRequest,
    TransferLeaderResponse, VoteRequest, VoteResponse,
};

use crate::state_machine::{SnapshotData, snapshot_meta_keys};
use crate::transport::{Request, Response, SnapshotEnd, Transport, TransportError};
use crate::types::{NodeId, TypeConfig};

/// Makes a [`Connection`] to each peer over one transport.
#[derive(Debug)]
pub struct NetworkFactory<T> {
    /// The transport every connection shares.
    transport: Arc<T>,
    /// Roughly how many bytes each snapshot chunk holds.
    chunk_bytes: usize,
}

impl<T> NetworkFactory<T> {
    /// Connect peers over `transport`, sending snapshots in chunks of
    /// about `chunk_bytes`.
    pub fn new(transport: Arc<T>, chunk_bytes: usize) -> Self {
        Self {
            transport,
            chunk_bytes: chunk_bytes.max(1),
        }
    }
}

impl<T: Transport> RaftNetworkFactory<TypeConfig> for NetworkFactory<T> {
    type Network = Connection<T>;

    /// Return a connection to node `target`.
    async fn new_client(&mut self, target: NodeId, _node: &openraft::BasicNode) -> Connection<T> {
        Connection {
            transport: Arc::clone(&self.transport),
            target,
            chunk_bytes: self.chunk_bytes,
        }
    }
}

/// Raft's view of one peer.
#[derive(Debug)]
pub struct Connection<T> {
    /// The transport.
    transport: Arc<T>,
    /// The peer.
    target: NodeId,
    /// Roughly how many bytes each snapshot chunk holds.
    chunk_bytes: usize,
}

/// Return an identifier for a new snapshot transfer, unique in practice.
fn transfer_id() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    RandomState::new().hash_one(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Convert a transport error to openraft's: a request that surely didn't
/// arrive means the peer is unreachable, and openraft backs off.
fn rpc_error(error: TransportError) -> RPCError<TypeConfig> {
    match error {
        TransportError::NotDelivered(m) => RPCError::Unreachable(Unreachable::from_string(m)),
        TransportError::NoReply(m) => RPCError::Network(NetworkError::from_string(m)),
    }
}

/// Return the error for a response of the wrong kind or a refusal.
fn unexpected(response: &Response) -> RPCError<TypeConfig> {
    match response {
        Response::Refused(m) => RPCError::Network(NetworkError::from_string(m)),
        other => RPCError::Network(NetworkError::from_string(format!(
            "unexpected response {other:?}"
        ))),
    }
}

impl<T: Transport> Connection<T> {
    /// Send a vote or pre-vote and return the peer's answer.
    async fn ballot(
        &self,
        request: Request,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        match self
            .transport
            .send(self.target, request)
            .await
            .map_err(rpc_error)?
        {
            Response::Vote(response) => Ok(response),
            other => Err(unexpected(&other)),
        }
    }
}

impl<T: Transport> RaftNetworkV2<TypeConfig> for Connection<T> {
    type SnapshotData = SnapshotData;

    /// Send AppendEntries and return the peer's answer.
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendResult<TypeConfig>, RPCError<TypeConfig>> {
        match self
            .transport
            .send(self.target, Request::Append(rpc))
            .await
            .map_err(rpc_error)?
        {
            Response::Append(result) => Ok(result.map_err(Into::into)),
            other => Err(unexpected(&other)),
        }
    }

    /// Send RequestVote and return the peer's answer.
    async fn vote(
        &mut self,
        rpc: VoteRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        self.ballot(Request::Vote(rpc)).await
    }

    /// Send a pre-vote and return the peer's answer.
    async fn pre_vote(
        &mut self,
        rpc: VoteRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        self.ballot(Request::PreVote(rpc)).await
    }

    /// Send a whole snapshot in one message, unless cancelled first.
    async fn full_snapshot(
        &mut self,
        vote: VoteOf<TypeConfig>,
        snapshot: SnapshotOf<TypeConfig, SnapshotData>,
        cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<TypeConfig>, StreamingError<TypeConfig>> {
        let SnapshotData::View(view) = snapshot.snapshot else {
            return Err(StreamingError::Network(NetworkError::from_string(
                "only a view of this replica can be sent",
            )));
        };
        let failed =
            |e: &dyn std::fmt::Display| StreamingError::Network(NetworkError::from_string(e));
        let id = transfer_id();
        let mut chunks = view.chunks(snapshot_meta_keys(), self.chunk_bytes);
        let mut cancel = std::pin::pin!(cancel);
        // Each chunk is exported when it is about to be sent, so the next is
        // read ahead to know which chunk is last.
        let mut next = chunks.next().await.map_err(|e| failed(&e))?;
        let mut end = Some(SnapshotEnd {
            vote,
            meta: snapshot.meta,
        });
        for seq in 0.. {
            let bytes = next.take().expect("a chunk is ready");
            next = chunks.next().await.map_err(|e| failed(&e))?;
            let last = next.is_none();
            let request = Request::SnapshotChunk {
                id,
                seq,
                bytes,
                end: if last { end.take() } else { None },
            };
            let response = tokio::select! {
                response = self.transport.send(self.target, request) => response,
                closed = &mut cancel => return Err(StreamingError::Closed(closed)),
            };
            match response {
                Ok(Response::SnapshotChunk(Ok(()))) if !last => {}
                Ok(Response::Snapshot(response)) if last => return Ok(response),
                Ok(Response::SnapshotChunk(Err(m))) => return Err(failed(&m)),
                Ok(other) => return Err(failed(&format!("unexpected response {other:?}"))),
                Err(TransportError::NotDelivered(m)) => {
                    return Err(StreamingError::Unreachable(Unreachable::from_string(m)));
                }
                Err(TransportError::NoReply(m)) => return Err(failed(&m)),
            }
        }
        unreachable!("the chunk loop returns")
    }

    /// Ask the peer to take over leadership.
    async fn transfer_leader(
        &mut self,
        req: TransferLeaderRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<TransferLeaderResponse<TypeConfig>, RPCError<TypeConfig>> {
        match self
            .transport
            .send(self.target, Request::TransferLeader(req))
            .await
            .map_err(rpc_error)?
        {
            Response::TransferLeader(response) => Ok(response),
            other => Err(unexpected(&other)),
        }
    }
}
