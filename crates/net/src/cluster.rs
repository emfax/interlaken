//! A cluster member over the production network (ADR-0037 phase 5).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use interlaken_replication::types::NodeId;
use interlaken_replication::{ClusterStatus, ReplicaConfig, ReplicaNode};
use interlaken_storage::{Error, FjallStorage, Result, StorageOptions};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::metrics::{Counters, NetMetrics};
use crate::server::{Limits, serve};
use crate::tls::{MembershipView, TlsConfig, TlsMaterial, TlsState};
use crate::transport::{AddressBook, GrpcTransport};

/// How a cluster member runs.
#[derive(Clone, Debug)]
pub struct ClusterConfig {
    /// This node's ID, unique in the cluster.
    pub node_id: NodeId,
    /// The address to listen on for other replicas.
    pub listen: SocketAddr,
    /// The address other replicas reach this node at, if not `listen` as bound.
    pub advertise: Option<String>,
    /// Other members' addresses as this node reaches them. They take
    /// precedence over the addresses membership records, which reach the
    /// members not listed here, so a node can reach peers through a proxy
    /// or a different network.
    pub peers: BTreeMap<NodeId, String>,
    /// Mutual TLS.
    pub tls: TlsConfig,
    /// Form a new cluster of this node alone, if it belongs to none yet.
    /// Other nodes join with [`Cluster::add_member`].
    pub bootstrap: bool,
    /// Raft and commit settings; the ID is `node_id`.
    pub replica: ReplicaConfig,
    /// The deadline for each request between replicas.
    pub request_timeout: Duration,
    /// Requests served at once on each connection; more are shed.
    pub concurrency: usize,
    /// How long a peer's connection to this member may live before it is
    /// closed and the peer must connect, and be verified, again (ADR-0041).
    pub max_connection_age: Duration,
}

impl ClusterConfig {
    /// Return the settings for node `node_id` listening on `listen`, with
    /// default Raft settings, 5-second request deadlines and 256 concurrent
    /// requests per connection.
    #[must_use]
    pub fn new(node_id: NodeId, listen: SocketAddr, tls: TlsConfig) -> Self {
        Self {
            node_id,
            listen,
            advertise: None,
            peers: BTreeMap::new(),
            tls,
            bootstrap: false,
            replica: ReplicaConfig::new(node_id),
            request_timeout: Duration::from_secs(5),
            concurrency: 256,
            max_connection_age: Duration::from_secs(3600),
        }
    }
}

/// A running cluster member: a replica, its gRPC server, and the address
/// book its transport uses.
pub struct Cluster {
    /// The replica.
    node: Arc<ReplicaNode<GrpcTransport>>,
    /// The address the server is bound to.
    local_addr: SocketAddr,
    /// The address peers reach this node at.
    advertise: String,
    /// Traffic counters.
    counters: Arc<Counters>,
    /// The TLS material, verifier and membership view.
    tls: Arc<TlsState>,
    /// Stops the server.
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    /// The server and address-book tasks.
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for Cluster {
    /// Render the member's ID and address.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cluster")
            .field("id", &self.node.id())
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

/// Return the error for a network setup failure.
fn unavailable(what: &str, error: &dyn std::fmt::Display) -> Error {
    Error::ReplicationUnavailable(format!("{what}: {error}"))
}

impl Cluster {
    /// Open the replica stored at `path`, serve other replicas on
    /// `config.listen`, and join the cluster: rejoin it if this node
    /// belongs to one, form one if `config.bootstrap` is set, or else wait
    /// to be added.
    ///
    /// # Errors
    ///
    /// A storage error, or [`Error::ReplicationUnavailable`] if the address
    /// can't be bound, TLS can't be set up, or Raft can't start.
    pub async fn open(
        path: PathBuf,
        storage: StorageOptions,
        config: ClusterConfig,
    ) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind(config.listen)
            .await
            .map_err(|e| unavailable("binding the listener", &e))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| unavailable("reading the bound address", &e))?;
        let advertise = config
            .advertise
            .clone()
            .unwrap_or_else(|| local_addr.to_string());
        let mut addresses = config.peers.clone();
        addresses.insert(config.node_id, advertise.clone());
        let book: AddressBook = Arc::new(RwLock::new(addresses));
        let counters = Arc::new(Counters::default());
        let tls = Arc::new(
            TlsState::new(&config.tls, config.node_id)
                .map_err(|e| unavailable("loading the TLS material", &e))?,
        );
        let transport = GrpcTransport::new(
            Arc::clone(&book),
            Arc::clone(&tls),
            config.request_timeout,
            Arc::clone(&counters),
        );
        let mut replica = config.replica;
        replica.id = config.node_id;
        let node = Arc::new(ReplicaNode::open(path, storage, replica, transport).await?);
        let (stop, stopped) = oneshot::channel::<()>();
        let limits = Limits {
            timeout: config.request_timeout,
            concurrency: config.concurrency.max(1),
            max_connection_age: config.max_connection_age,
        };
        let server = tokio::spawn({
            let tls = Arc::clone(&tls);
            let handler = node.handler();
            let counters = Arc::clone(&counters);
            async move {
                let shutdown = async {
                    let _ = stopped.await;
                };
                if let Err(e) = serve(listener, tls, handler, counters, limits, shutdown).await {
                    eprintln!("interlaken-net: the replica server stopped: {e}");
                }
            }
        });
        // Membership carries every member's address; keep the book current
        // for members without a configured address.
        let configured: Vec<NodeId> = config
            .peers
            .keys()
            .copied()
            .chain([config.node_id])
            .collect();
        let updater = tokio::spawn({
            let node = Arc::clone(&node);
            let tls = Arc::clone(&tls);
            async move {
                loop {
                    // The verifier checks peers against the membership.
                    let status = node.status();
                    tls.set_membership(MembershipView {
                        known: !status.voters.is_empty(),
                        voters: status.voters.iter().copied().collect(),
                        learners: status.learners.iter().copied().collect(),
                    });
                    tls.record_expiry();
                    let known = node.addresses();
                    {
                        let mut entries = book.write().unwrap_or_else(PoisonError::into_inner);
                        for (id, addr) in known {
                            if !configured.contains(&id) {
                                entries.insert(id, addr);
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        });
        let cluster = Self {
            node,
            local_addr,
            advertise,
            counters,
            tls,
            shutdown: Mutex::new(Some(stop)),
            tasks: Mutex::new(vec![server, updater]),
        };
        if config.bootstrap {
            // Stop everything on failure: a replica left running would keep
            // the storage locked against the next attempt.
            if let Err(e) = cluster.bootstrap(config.node_id).await {
                cluster.shutdown().await;
                return Err(e);
            }
        }
        Ok(cluster)
    }

    /// Form a cluster of this node alone, unless it belongs to one already.
    async fn bootstrap(&self, id: NodeId) -> Result<()> {
        let initialized = self
            .node
            .raft()
            .is_initialized()
            .await
            .map_err(|e| unavailable("reading Raft's state", &e))?;
        if !initialized {
            let members = BTreeMap::from([(id, self.advertise.clone())]);
            self.node.initialize_at(&members).await?;
        }
        Ok(())
    }

    /// Return the replica.
    #[must_use]
    pub fn node(&self) -> &ReplicaNode<GrpcTransport> {
        &self.node
    }

    /// Return the replica's storage.
    #[must_use]
    pub fn storage(&self) -> &FjallStorage {
        self.node.storage()
    }

    /// Return the address the server is bound to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Return this replica's view of the cluster.
    #[must_use]
    pub fn status(&self) -> ClusterStatus {
        self.node.status()
    }

    /// Replace this member's certificate and key and the trusted CAs while
    /// running (ADR-0041).
    ///
    /// The new material is checked first: the key must match the
    /// certificate, and the certificate must chain to one of the CAs. New
    /// connections use it at once; this member remakes its connections to
    /// peers, and peers' connections to it are remade within
    /// [`ClusterConfig::max_connection_age`]. To rotate the CA itself,
    /// trust the old and new CAs together while certificates move from one
    /// to the other, then drop the old one.
    ///
    /// # Errors
    ///
    /// [`Error::ReplicationUnavailable`] naming what is wrong with the
    /// material, which then isn't used.
    pub fn rotate_tls(&self, material: &TlsMaterial) -> Result<()> {
        self.tls
            .rotate(material)
            .map_err(|e| unavailable("rotating the TLS material", &e))
    }

    /// Return the network counters.
    #[must_use]
    pub fn net_metrics(&self) -> NetMetrics {
        self.counters.snapshot()
    }

    /// Wait until this node knows a leader, and return it.
    ///
    /// # Errors
    ///
    /// [`Error::ReplicationUnavailable`] if none is known within `timeout`.
    pub async fn wait_for_leader(&self, timeout: Duration) -> Result<NodeId> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(leader) = self.node.leader() {
                return Ok(leader);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::ReplicationUnavailable("no leader elected".into()));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Add node `id`, reachable at `addr`, as a voter: first as a learner,
    /// which catches up, then into the voters.
    ///
    /// On the leader, the first addition snapshots and compacts the log, so
    /// the new member starts from a snapshot: a cluster bootstrapped from an
    /// existing database holds data no log entry carries.
    ///
    /// # Errors
    ///
    /// As [`ReplicaNode::change_membership`].
    pub async fn add_member(&self, id: NodeId, addr: &str) -> Result<()> {
        let status = self.node.status();
        if status.leader == Some(self.node.id()) && status.snapshot_index.is_none() {
            self.node.compact().await?;
        }
        self.node.add_learner_at(id, addr).await?;
        let mut voters = self.node.status().voters;
        if !voters.contains(&id) {
            voters.push(id);
        }
        self.node.set_voters(&voters).await
    }

    /// Remove node `id` from the voters, and so from the cluster.
    ///
    /// # Errors
    ///
    /// As [`ReplicaNode::change_membership`].
    pub async fn remove_member(&self, id: NodeId) -> Result<()> {
        let voters: Vec<NodeId> = self
            .node
            .status()
            .voters
            .into_iter()
            .filter(|&v| v != id)
            .collect();
        self.node.set_voters(&voters).await
    }

    /// Hand leadership to node `to`.
    ///
    /// # Errors
    ///
    /// As [`ReplicaNode::change_membership`].
    pub async fn transfer_leadership(&self, to: NodeId) -> Result<()> {
        self.node.transfer_leadership(to).await
    }

    /// Stop serving and stop Raft. The storage stays readable.
    pub async fn shutdown(&self) {
        if let Some(stop) = self
            .shutdown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = stop.send(());
        }
        self.node.shutdown().await;
        for task in self
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
    }
}
