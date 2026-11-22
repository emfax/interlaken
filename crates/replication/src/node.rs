//! A replica: storage, Raft and the commit path together (ADR-0037).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use interlaken_storage::replica::{CommitPath, Replica};
use interlaken_storage::{CommitRequest, Durability, Error, FjallStorage, Result, StorageOptions};
use openraft::async_runtime::WatchReceiver;
use openraft::{BasicNode, SnapshotPolicy};
use tokio::sync::watch;

use crate::commit::{CommitConfig, Outcome, RaftCommit};
use crate::log_store::LogStore;
use crate::network::NetworkFactory;
use crate::state_machine::StateMachine;
use crate::transport::{
    CommitError, Committed, Handler, HandlerFuture, Request, Response, Transport,
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
}

impl ReplicaConfig {
    /// Return the settings for node `id`: 50 ms heartbeats, elections after
    /// 150 to 300 ms, and the default commit settings.
    #[must_use]
    pub fn new(id: NodeId) -> Self {
        Self {
            id,
            heartbeat: Duration::from_millis(50),
            election_min: Duration::from_millis(150),
            election_max: Duration::from_millis(300),
            commit: CommitConfig::default(),
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
    /// The commit path.
    commit: Arc<RaftCommit<T>>,
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
            // Snapshots come with phase 4; until then the log is kept whole.
            snapshot_policy: SnapshotPolicy::Never,
            ..openraft::Config::default()
        }
        .validate()
        .map_err(|e| Error::ReplicationUnavailable(e.to_string()))?;
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
        commit.start(raft.clone(), replica, applied, transport);
        Ok(Self {
            id: config.id,
            raft,
            storage,
            commit,
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
        let members: BTreeMap<NodeId, BasicNode> = members
            .iter()
            .map(|&id| (id, BasicNode::new(id.to_string())))
            .collect();
        self.raft
            .initialize(members)
            .await
            .map_err(|e| Error::ReplicationUnavailable(e.to_string()))
    }

    /// Return a handler that answers other replicas' requests to this node.
    #[must_use]
    pub fn handler(&self) -> Arc<dyn Handler> {
        Arc::new(NodeHandler {
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
