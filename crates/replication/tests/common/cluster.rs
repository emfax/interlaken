//! A cluster of replicas in one process, over the in-memory transport.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use interlaken_replication::{MemoryNetwork, MemoryTransport, ReplicaConfig, ReplicaNode};
use interlaken_storage::{Error, Freshness, Storage, StorageOptions, Timestamp, TxnOptions};
use tempfile::TempDir;

/// A cluster of replicas in temporary directories.
pub struct Cluster {
    /// The network connecting them.
    pub network: Arc<MemoryNetwork>,
    /// Each node's directory, kept across restarts.
    pub dirs: BTreeMap<u64, TempDir>,
    /// The running nodes.
    pub nodes: BTreeMap<u64, ReplicaNode<MemoryTransport>>,
    /// Every member's ID.
    pub ids: Vec<u64>,
    /// Adjusts each node's settings.
    configure: fn(&mut ReplicaConfig),
}

/// Return the settings tests use: short commit timeouts, so unavailability
/// shows quickly, and elections slow enough that a loaded machine running
/// many clusters at once doesn't depose leaders by accident.
fn config(id: u64) -> ReplicaConfig {
    let mut config = ReplicaConfig::new(id);
    config.heartbeat = Duration::from_millis(100);
    config.election_min = Duration::from_millis(500);
    config.election_max = Duration::from_millis(1000);
    config.commit.timeout = Duration::from_secs(4);
    config
}

/// Keep the test settings as they are.
fn unchanged(_: &mut ReplicaConfig) {}

impl Cluster {
    /// Start `n` nodes and form a cluster of them.
    pub async fn start(n: u64) -> Self {
        Self::start_with(n, unchanged).await
    }

    /// Start `n` nodes whose settings `configure` adjusts, and form a cluster of them.
    pub async fn start_with(n: u64, configure: fn(&mut ReplicaConfig)) -> Self {
        let mut cluster = Self {
            network: MemoryNetwork::new(),
            dirs: BTreeMap::new(),
            nodes: BTreeMap::new(),
            ids: (1..=n).collect(),
            configure,
        };
        for id in 1..=n {
            cluster.dirs.insert(id, tempfile::tempdir().unwrap());
            cluster.boot(id).await;
        }
        cluster.nodes[&1].initialize(&cluster.ids).await.unwrap();
        cluster.leader().await;
        cluster
    }

    /// Open node `id` from its directory and connect it.
    pub async fn boot(&mut self, id: u64) {
        let mut config = config(id);
        (self.configure)(&mut config);
        let node = ReplicaNode::open(
            self.dirs[&id].path().to_path_buf(),
            StorageOptions::default(),
            config,
            self.network.transport(id),
        )
        .await
        .unwrap();
        self.network.register(id, node.handler());
        self.nodes.insert(id, node);
    }

    /// Start node `id` in a new directory, outside the cluster until added.
    pub async fn add_node(&mut self, id: u64) {
        self.dirs.insert(id, tempfile::tempdir().unwrap());
        self.ids.push(id);
        self.boot(id).await;
    }

    /// Stop node `id`, as if it crashed, keeping its directory.
    pub async fn crash(&mut self, id: u64) {
        self.network.unregister(id);
        let node = self.nodes.remove(&id).unwrap();
        node.shutdown().await;
        drop(node);
        // Let background tasks holding the storage finish.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Return the leader, once the running nodes agree on one.
    pub async fn leader(&self) -> u64 {
        for _ in 0..400 {
            let leaders: Vec<_> = self.nodes.values().map(ReplicaNode::leader).collect();
            if let Some(Some(leader)) = leaders.first()
                && self.nodes.contains_key(leader)
                && leaders.iter().all(|l| l == &Some(*leader))
            {
                return *leader;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no leader elected");
    }

    /// Return a running node other than `leader`.
    pub fn follower(&self, leader: u64) -> u64 {
        *self.nodes.keys().find(|&&id| id != leader).unwrap()
    }

    /// Put `key` = `value` in one transaction on node `id`.
    pub async fn put(&self, id: u64, key: &str, value: &str) -> Result<Timestamp, Error> {
        let storage = self.nodes[&id].storage();
        let txn = storage.begin(TxnOptions::default()).await?;
        txn.put(
            Bytes::copy_from_slice(key.as_bytes()),
            Bytes::copy_from_slice(value.as_bytes()),
        )?;
        Ok(txn.commit().await?.commit_ts.unwrap())
    }

    /// Return `key`'s value on node `id`, once it has applied `ts`.
    pub async fn get(&self, id: u64, key: &str, ts: Timestamp) -> Option<String> {
        let storage = self.nodes[&id].storage();
        for _ in 0..400 {
            if storage.visible_ts() >= ts {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(storage.visible_ts() >= ts, "node {id} applied {ts:?}");
        let txn = storage
            .begin(TxnOptions::default().read_only())
            .await
            .unwrap();
        let values = txn
            .get_many(vec![Bytes::copy_from_slice(key.as_bytes())])
            .await
            .unwrap();
        values[0]
            .as_ref()
            .map(|v| String::from_utf8(v.to_vec()).unwrap())
    }

    /// Read `key` on node `id` in a read-only transaction with `freshness`.
    pub async fn read(
        &self,
        id: u64,
        key: &str,
        freshness: Freshness,
    ) -> Result<Option<String>, Error> {
        let storage = self.nodes[&id].storage();
        let txn = storage
            .begin(TxnOptions::default().read_only().freshness(freshness))
            .await?;
        let values = txn
            .get_many(vec![Bytes::copy_from_slice(key.as_bytes())])
            .await?;
        Ok(values[0]
            .as_ref()
            .map(|v| String::from_utf8(v.to_vec()).unwrap()))
    }
}
