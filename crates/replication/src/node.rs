//! A replica: storage, Raft and the commit path together (ADR-0037).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use interlaken_storage::replica::{CommitPath, Replica};
use interlaken_storage::{CommitRequest, Durability, Error, FjallStorage, Result, StorageOptions};
use openraft::async_runtime::WatchReceiver;
use openraft::error::ClientWriteError;
use openraft::impls::ChangeMembers;
use openraft::{BasicNode, ServerState, SnapshotPolicy};
use tokio::sync::watch;
use tokio::time::{Instant, sleep};

use crate::commit::{CommitConfig, Outcome, RaftCommit};
use crate::log_store::LogStore;
use crate::network::NetworkFactory;
use crate::state_machine::StateMachine;
use crate::transport::{
    CommitError, Committed, Handler, HandlerFuture, MembershipChange, Request, Response, Transport,
    TransportError,
};
use crate::types::{NodeId, Raft};

/// How a replica runs.
#[derive(Clone, Copy, Debug)]
pub struct ReplicaConfig {
    /// This node's ID, unique in the cluster.
    pub id: NodeId,
    /// How often the leader sends heartbeats.
    pub heartbeat: Duration,
    /// The shortest time without a leader before a follower stands for election.
    pub election_min: Duration,
    /// The longest time without a leader before a follower stands for election.
    pub election_max: Duration,
    /// How commits are grouped and retried.
    pub commit: CommitConfig,
    /// How long applied commits stay in the validation history, so that
    /// transactions on other replicas whose snapshots are that old can be
    /// validated once this node leads.
    pub history_retention: Duration,
    /// Build a snapshot after this many log entries since the last one.
    pub snapshot_every: u64,
    /// Log entries kept after a snapshot, so replicas slightly behind catch
    /// up from the log rather than from a snapshot.
    pub logs_kept: u64,
}

impl ReplicaConfig {
    /// Return the settings for node `id`: 50 ms heartbeats, elections after
    /// 150 to 300 ms, the default commit settings, a minute's history, and a
    /// snapshot every 10,000 entries keeping the last 1,000.
    #[must_use]
    pub fn new(id: NodeId) -> Self {
        Self {
            id,
            heartbeat: Duration::from_millis(50),
            election_min: Duration::from_millis(150),
            election_max: Duration::from_millis(300),
            commit: CommitConfig::default(),
            history_retention: Duration::from_secs(60),
            snapshot_every: 10_000,
            logs_kept: 1_000,
        }
    }
}

/// Return a duration in whole milliseconds, as openraft's settings take them.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// One replica of a replicated database.
pub struct ReplicaNode<T: Transport> {
    /// This node.
    id: NodeId,
    /// The Raft handle.
    raft: Raft,
    /// The replica's storage, which commits through Raft.
    storage: FjallStorage,
    /// Storage's side of the replica.
    replica: Replica,
    /// The commit path.
    commit: Arc<RaftCommit<T>>,
    /// How long a membership change may wait for a leader.
    timeout: Duration,
    /// How long to wait between attempts to reach a leader.
    retry: Duration,
}

/// A replica's view of the cluster (ADR-0037 phase 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterStatus {
    /// This node.
    pub id: NodeId,
    /// This node's role: `leader`, `follower`, `candidate` or `learner`.
    pub role: &'static str,
    /// The current term.
    pub term: u64,
    /// The node this replica believes leads, if any.
    pub leader: Option<NodeId>,
    /// The index of the last log entry here.
    pub last_log_index: Option<u64>,
    /// The index of the last entry known committed.
    pub committed_index: Option<u64>,
    /// The index of the last entry applied here.
    pub applied_index: Option<u64>,
    /// The index the latest snapshot covers, if any.
    pub snapshot_index: Option<u64>,
    /// The voters, in the latest membership.
    pub voters: Vec<NodeId>,
    /// The learners, in the latest membership.
    pub learners: Vec<NodeId>,
    /// On the leader, how many entries each other member's log trails the
    /// leader's by, or `None` for a member not yet heard from.
    pub lag: BTreeMap<NodeId, Option<u64>>,
}

/// Make `change` on this node, if it leads.
async fn change_here(
    raft: &Raft,
    id: NodeId,
    change: MembershipChange,
) -> std::result::Result<(), CommitError> {
    let refused = |e: openraft::error::RaftError<_, ClientWriteError<_>>| match e.api_error() {
        Some(ClientWriteError::ForwardToLeader(_)) => CommitError::NotLeader,
        _ => CommitError::Unavailable(e.to_string()),
    };
    match change {
        MembershipChange::AddLearner { id: node, addr } => raft
            .add_learner(node, BasicNode::new(addr), true)
            .await
            .map(drop)
            .map_err(refused),
        MembershipChange::SetVoters { voters, retain } => raft
            .change_membership(
                ChangeMembers::ReplaceAllVoters(voters.into_iter().collect()),
                retain,
            )
            .await
            .map(drop)
            .map_err(refused),
        MembershipChange::RemoveLearners(nodes) => raft
            .change_membership(
                ChangeMembers::RemoveNodes(nodes.into_iter().collect::<BTreeSet<_>>()),
                false,
            )
            .await
            .map(drop)
            .map_err(refused),
        MembershipChange::TransferLeadership(to) => {
            if raft.metrics().borrow_watched().current_leader != Some(id) {
                return Err(CommitError::NotLeader);
            }
            raft.trigger()
                .transfer_leader(to)
                .await
                .map_err(|e| CommitError::Unavailable(e.to_string()))
        }
    }
}

impl<T: Transport> std::fmt::Debug for ReplicaNode<T> {
    /// Render the node's ID.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicaNode")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl<T: Transport> ReplicaNode<T> {
    /// Open the replica stored at `path` and start Raft, talking to peers
    /// over `transport`. A new replica waits to be initialized or added to
    /// a cluster; an existing one rejoins its cluster.
    ///
    /// `options.durability` decides whether Raft's log and vote are synced
    /// before they are acknowledged: under `Durability::Buffered`, a power
    /// loss on a majority of nodes at once can lose acknowledged commits.
    ///
    /// # Errors
    ///
    /// Returns a storage error, or [`Error::ReplicationUnavailable`] if Raft
    /// can't start.
    pub async fn open(
        path: PathBuf,
        options: StorageOptions,
        config: ReplicaConfig,
        transport: T,
    ) -> Result<Self> {
        let sync = matches!(options.durability, Durability::Sync);
        let commit = RaftCommit::<T>::new(config.id, config.commit);
        let path_commit = Arc::clone(&commit);
        let opened = tokio::task::spawn_blocking(move || -> Result<(FjallStorage, Replica)> {
            let mut replica = None;
            let storage = FjallStorage::open_replicated(&path, &options, |r| {
                replica = Some(r);
                Ok(path_commit as Arc<dyn CommitPath>)
            })?;
            Ok((
                storage,
                replica.expect("open_replicated hands over the replica"),
            ))
        })
        .await
        .map_err(|e| Error::ReplicationUnavailable(format!("opening storage panicked: {e}")))?;
        let (storage, replica) = opened?;
        let raft_config = openraft::Config {
            cluster_name: "interlaken".into(),
            heartbeat_interval: millis(config.heartbeat),
            election_timeout_min: millis(config.election_min),
            election_timeout_max: millis(config.election_max),
            snapshot_policy: SnapshotPolicy::LogsSinceLast(config.snapshot_every.max(1)),
            max_in_snapshot_log_to_keep: config.logs_kept,
            ..openraft::Config::default()
        }
        .validate()
        .map_err(|e| Error::ReplicationUnavailable(e.to_string()))?;
        replica.set_history_retention(config.history_retention);
        let (applied_tx, applied) = watch::channel(0);
        let transport = Arc::new(transport);
        let raft = Raft::new(
            config.id,
            Arc::new(raft_config),
            NetworkFactory::new(Arc::clone(&transport)),
            LogStore::new(replica.clone(), sync),
            StateMachine::new(replica.clone(), applied_tx),
        )
        .await
        .map_err(|e| Error::ReplicationUnavailable(e.to_string()))?;
        commit.start(raft.clone(), replica.clone(), applied, transport);
        Ok(Self {
            id: config.id,
            raft,
            storage,
            replica,
            commit,
            timeout: config.commit.timeout,
            retry: config.commit.retry,
        })
    }

    /// Return this node's ID.
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Return the replica's storage. Transactions begun on it read this
    /// replica's applied state and commit through the leader.
    #[must_use]
    pub fn storage(&self) -> &FjallStorage {
        &self.storage
    }

    /// Return the node this replica believes leads, if any.
    #[must_use]
    pub fn leader(&self) -> Option<NodeId> {
        self.raft.metrics().borrow_watched().current_leader
    }

    /// Return the Raft handle, for metrics and cluster changes.
    #[must_use]
    pub fn raft(&self) -> &Raft {
        &self.raft
    }

    /// Form a new cluster of `members`, from this node. Call it on one
    /// node, once, for a cluster's first members.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReplicationUnavailable`] if this node already
    /// belongs to a cluster.
    pub async fn initialize(&self, members: &[NodeId]) -> Result<()> {
        let members: BTreeMap<NodeId, String> =
            members.iter().map(|&id| (id, id.to_string())).collect();
        self.initialize_at(&members).await
    }

    /// Form a new cluster of `members`, each reachable at its address, from
    /// this node. Call it on one node, once.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReplicationUnavailable`] if this node already
    /// belongs to a cluster.
    pub async fn initialize_at(&self, members: &BTreeMap<NodeId, String>) -> Result<()> {
        let members: BTreeMap<NodeId, BasicNode> = members
            .iter()
            .map(|(&id, addr)| (id, BasicNode::new(addr.clone())))
            .collect();
        self.raft
            .initialize(members)
            .await
            .map_err(|e| Error::ReplicationUnavailable(e.to_string()))
    }

    /// Make a membership change through the leader, retrying until a
    /// leader takes it or the commit timeout passes. Changes use joint
    /// consensus, so the cluster stays available throughout.
    ///
    /// # Errors
    ///
    /// [`Error::ReplicationUnavailable`] if no leader took the change, or
    /// the leader refused it, such as a voter set without a learner that
    /// has caught up.
    pub async fn change_membership(&self, change: MembershipChange) -> Result<()> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let leader = self.leader();
            let outcome = match leader {
                Some(leader) if leader == self.id => {
                    // Adding a learner waits for it to catch up, which a
                    // learner that can't be reached never does.
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    match tokio::time::timeout(
                        remaining,
                        change_here(&self.raft, self.id, change.clone()),
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(_) => Err(CommitError::Unavailable(
                            "the membership change didn't complete in time".into(),
                        )),
                    }
                }
                Some(leader) => match self
                    .commit
                    .transport()
                    .send(leader, Request::Membership(change.clone()))
                    .await
                {
                    Ok(Response::Membership(outcome)) => outcome,
                    Ok(_) | Err(TransportError::NotDelivered(_) | TransportError::NoReply(_)) => {
                        Err(CommitError::NotLeader)
                    }
                },
                None => Err(CommitError::NotLeader),
            };
            match outcome {
                Ok(()) => return Ok(()),
                Err(CommitError::NotLeader) if Instant::now() < deadline => sleep(self.retry).await,
                Err(CommitError::NotLeader) => {
                    return Err(Error::ReplicationUnavailable(
                        "no leader took the membership change".into(),
                    ));
                }
                Err(other) => return Err(Error::ReplicationUnavailable(format!("{other:?}"))),
            }
        }
    }

    /// Add node `id` as a learner, returning once it has caught up. Its
    /// address is its ID, as in-process transports resolve it.
    ///
    /// # Errors
    ///
    /// As [`ReplicaNode::change_membership`].
    pub async fn add_learner(&self, id: NodeId) -> Result<()> {
        self.add_learner_at(id, &id.to_string()).await
    }

    /// Add node `id`, reachable at `addr`, as a learner, returning once it
    /// has caught up.
    ///
    /// # Errors
    ///
    /// As [`ReplicaNode::change_membership`].
    pub async fn add_learner_at(&self, id: NodeId, addr: &str) -> Result<()> {
        self.change_membership(MembershipChange::AddLearner {
            id,
            addr: addr.to_owned(),
        })
        .await
    }

    /// Return every member's address, from the latest membership.
    #[must_use]
    pub fn addresses(&self) -> BTreeMap<NodeId, String> {
        self.raft
            .metrics()
            .borrow_watched()
            .membership_config
            .membership()
            .nodes()
            .map(|(id, node)| (*id, node.addr.clone()))
            .collect()
    }

    /// Build a snapshot of everything applied here and purge the log it
    /// covers, so replicas joining later start from the snapshot. A cluster
    /// bootstrapped from an existing database needs this: its data predates
    /// the log.
    ///
    /// # Errors
    ///
    /// [`Error::ReplicationUnavailable`] if Raft has stopped or the
    /// snapshot isn't built within the commit timeout.
    pub async fn compact(&self) -> Result<()> {
        let unavailable = |e: &dyn std::fmt::Display| Error::ReplicationUnavailable(e.to_string());
        let applied = self.status().applied_index.unwrap_or(0);
        self.raft
            .trigger()
            .snapshot()
            .await
            .map_err(|e| unavailable(&e))?;
        self.raft
            .wait(Some(self.timeout))
            .metrics(
                |m| m.snapshot.as_ref().is_some_and(|s| s.index() >= applied),
                "the snapshot covers everything applied",
            )
            .await
            .map_err(|e| unavailable(&e))?;
        let covered = self.status().snapshot_index.unwrap_or(0);
        self.raft
            .trigger()
            .purge_log(covered)
            .await
            .map_err(|e| unavailable(&e))
    }

    /// Make exactly `voters` the voting members. Nodes added must be
    /// learners already; voters removed leave the cluster.
    ///
    /// # Errors
    ///
    /// As [`ReplicaNode::change_membership`].
    pub async fn set_voters(&self, voters: &[NodeId]) -> Result<()> {
        self.change_membership(MembershipChange::SetVoters {
            voters: voters.to_vec(),
            retain: false,
        })
        .await
    }

    /// Hand leadership to node `to`, which must be a voter.
    ///
    /// # Errors
    ///
    /// As [`ReplicaNode::change_membership`].
    pub async fn transfer_leadership(&self, to: NodeId) -> Result<()> {
        self.change_membership(MembershipChange::TransferLeadership(to))
            .await
    }

    /// Return the bytes of snapshot data this replica stores: none, except
    /// while it installs a snapshot it received (ADR-0038).
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn stored_snapshot_bytes(&self) -> Result<usize> {
        Ok(self
            .replica
            .stored_snapshot()
            .await?
            .map_or(0, |blob| blob.len()))
    }

    /// Return this replica's view of the cluster.
    #[must_use]
    pub fn status(&self) -> ClusterStatus {
        let metrics = self.raft.metrics().borrow_watched().clone();
        let membership = metrics.membership_config.membership();
        let voters: Vec<NodeId> = membership.voter_ids().collect();
        let learners: Vec<NodeId> = membership.non_write_voter_ids().collect();
        let last = metrics.last_log_index;
        let lag = metrics
            .replication
            .as_ref()
            .map(|replication| {
                replication
                    .iter()
                    .filter(|(id, _)| **id != self.id)
                    .map(|(id, matched)| {
                        let matched = matched.as_ref().map(openraft::LogId::index);
                        (*id, matched.zip(last).map(|(m, l)| l.saturating_sub(m)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        ClusterStatus {
            id: self.id,
            role: match metrics.state {
                ServerState::Leader => "leader",
                ServerState::Candidate => "candidate",
                ServerState::Learner => "learner",
                _ => "follower",
            },
            term: metrics.current_term,
            leader: metrics.current_leader,
            last_log_index: last,
            committed_index: metrics.local_committed.as_ref().map(openraft::LogId::index),
            applied_index: metrics.last_applied.as_ref().map(openraft::LogId::index),
            snapshot_index: metrics.snapshot.as_ref().map(openraft::LogId::index),
            voters,
            learners,
            lag,
        }
    }

    /// Return a handler that answers other replicas' requests to this node.
    #[must_use]
    pub fn handler(&self) -> Arc<dyn Handler> {
        Arc::new(NodeHandler {
            id: self.id,
            raft: self.raft.clone(),
            commit: Arc::clone(&self.commit),
        })
    }

    /// Stop Raft. Commits in flight fail; the storage stays readable.
    pub async fn shutdown(&self) {
        let _ = self.raft.shutdown().await;
    }
}

/// Answers other replicas' requests to one node.
struct NodeHandler<T: Transport> {
    /// The node.
    id: NodeId,
    /// The node's Raft handle.
    raft: Raft,
    /// The node's commit path.
    commit: Arc<RaftCommit<T>>,
}

impl<T: Transport> Handler for NodeHandler<T> {
    /// Pass Raft's messages to Raft, and forwarded commits to the proposer.
    fn handle(&self, request: Request) -> HandlerFuture {
        let raft = self.raft.clone();
        let commit = Arc::clone(&self.commit);
        let id = self.id;
        Box::pin(async move {
            match request {
                Request::Append(rpc) => match raft.append_entries(rpc).await {
                    Ok(result) => Response::Append(result.map_err(Into::into)),
                    Err(e) => Response::Refused(e.to_string()),
                },
                Request::Vote(rpc) => match raft.vote(rpc).await {
                    Ok(response) => Response::Vote(response),
                    Err(e) => Response::Refused(e.to_string()),
                },
                Request::PreVote(rpc) => match raft.pre_vote(rpc).await {
                    Ok(response) => Response::Vote(response),
                    Err(e) => Response::Refused(e.to_string()),
                },
                Request::ReadIndex => Response::ReadIndex(commit.remote_read_index().await),
                Request::Snapshot { vote, meta, data } => {
                    let snapshot = openraft::alias::SnapshotOf::<crate::types::TypeConfig, _> {
                        meta,
                        snapshot: crate::state_machine::SnapshotData::Received(data),
                    };
                    match raft.install_full_snapshot(vote, snapshot).await {
                        Ok(response) => Response::Snapshot(response),
                        Err(e) => Response::Refused(e.to_string()),
                    }
                }
                Request::TransferLeader(req) => match raft.handle_transfer_leader(req).await {
                    Ok(response) => Response::TransferLeader(response),
                    Err(e) => Response::Refused(e.to_string()),
                },
                Request::Membership(change) => {
                    Response::Membership(change_here(&raft, id, change).await)
                }
                Request::Commit(bytes) => {
                    let Ok(request) = CommitRequest::decode(&bytes) else {
                        return Response::Commit(Err(CommitError::Unavailable(
                            "malformed commit request".into(),
                        )));
                    };
                    Response::Commit(match commit.local(request).await {
                        Outcome::Committed(index, ts) => Ok(Committed {
                            index,
                            ts: ts.into_raw(),
                        }),
                        Outcome::NotLeader(_) => Err(CommitError::NotLeader),
                        Outcome::Failed(error) => Err(error),
                    })
                }
            }
        })
    }
}
