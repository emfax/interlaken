//! openraft's network, over a [`Transport`].

use std::future::Future;
use std::sync::Arc;

use openraft::alias::{SnapshotOf, VoteOf};
use openraft::error::{NetworkError, RPCError, ReplicationClosed, StreamingError, Unreachable};
use openraft::network::{RPCOption, RaftNetworkFactory, RaftNetworkV2};
use openraft::raft::{
    AppendEntriesRequest, AppendResult, SnapshotResponse, VoteRequest, VoteResponse,
};

use crate::state_machine::SnapshotData;
use crate::transport::{Request, Response, Transport, TransportError};
use crate::types::{NodeId, TypeConfig};

/// Makes a [`Connection`] to each peer over one transport.
#[derive(Debug)]
pub struct NetworkFactory<T> {
    /// The transport every connection shares.
    transport: Arc<T>,
}

impl<T> NetworkFactory<T> {
    /// Connect peers over `transport`.
    pub fn new(transport: Arc<T>) -> Self {
        Self { transport }
    }
}

impl<T: Transport> RaftNetworkFactory<TypeConfig> for NetworkFactory<T> {
    type Network = Connection<T>;

    /// Return a connection to node `target`.
    async fn new_client(&mut self, target: NodeId, _node: &openraft::BasicNode) -> Connection<T> {
        Connection {
            transport: Arc::clone(&self.transport),
            target,
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

    /// Refuse: snapshots come with phase 4, and without them the log is
    /// never purged, so a peer always catches up from it.
    async fn full_snapshot(
        &mut self,
        _vote: VoteOf<TypeConfig>,
        _snapshot: SnapshotOf<TypeConfig, SnapshotData>,
        _cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<TypeConfig>, StreamingError<TypeConfig>> {
        Err(StreamingError::Unreachable(Unreachable::from_string(
            "snapshots are not supported yet (ADR-0037 phase 4)",
        )))
    }
}
