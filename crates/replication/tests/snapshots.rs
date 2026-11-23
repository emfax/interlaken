//! Snapshot transfers in chunks (ADR-0038 phase 3).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use interlaken_replication::{
    MemoryNetwork, MemoryTransport, ReplicaConfig, ReplicaNode, Request, Response, Transport,
    TransportError,
};
use interlaken_storage::{Storage, StorageOptions, TxnOptions};

/// A transport that loses the reply to one chosen snapshot chunk.
struct LosesOneChunk {
    /// The real transport.
    inner: MemoryTransport,
    /// Snapshot chunks sent so far.
    chunks: Arc<AtomicU64>,
    /// Which chunk, counting from 0 across transfers, loses its reply.
    lose: u64,
}

impl Transport for LosesOneChunk {
    /// Send `request`, but report no reply for the chosen snapshot chunk.
    async fn send(&self, target: u64, request: Request) -> Result<Response, TransportError> {
        if matches!(request, Request::SnapshotChunk { .. })
            && self.chunks.fetch_add(1, Ordering::Relaxed) == self.lose
        {
            return Err(TransportError::NoReply("lost on purpose".into()));
        }
        self.inner.send(target, request).await
    }
}

/// Return settings for node `id`: small snapshot chunks, frequent compaction.
fn config(id: u64) -> ReplicaConfig {
    let mut config = ReplicaConfig::new(id);
    config.heartbeat = Duration::from_millis(100);
    config.election_min = Duration::from_millis(500);
    config.election_max = Duration::from_millis(1000);
    config.commit.timeout = Duration::from_secs(10);
    config.snapshot_every = 20;
    config.logs_kept = 1;
    config.snapshot_chunk_bytes = 512;
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a snapshot transfer interrupted midway is retried from the start
/// and completes, the new replica holding the leader's data.
async fn interrupted_transfers_are_retried() {
    let network = MemoryNetwork::new();
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let chunks = Arc::new(AtomicU64::new(0));
    let flaky = LosesOneChunk {
        inner: network.transport(1),
        chunks: Arc::clone(&chunks),
        lose: 5,
    };
    let first = ReplicaNode::open(
        dirs[0].path().to_path_buf(),
        StorageOptions::default(),
        config(1),
        flaky,
    )
    .await
    .unwrap();
    network.register(1, first.handler());
    first.initialize(&[1]).await.unwrap();
    while first.leader() != Some(1) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for i in 0..100 {
        let txn = first.storage().begin(TxnOptions::default()).await.unwrap();
        txn.put(
            Bytes::from(format!("k{i:03}")),
            Bytes::from(format!("value {i}")),
        )
        .unwrap();
        txn.commit().await.unwrap();
    }
    while first.status().snapshot_index.is_none() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let second = ReplicaNode::open(
        dirs[1].path().to_path_buf(),
        StorageOptions::default(),
        config(2),
        network.transport(2),
    )
    .await
    .unwrap();
    network.register(2, second.handler());
    first.add_learner(2).await.unwrap();
    let sent = chunks.load(Ordering::Relaxed);
    assert!(sent > 10, "only {sent} chunks were sent");
    assert!(
        second.status().snapshot_index.is_some(),
        "the new replica caught up from a snapshot"
    );
    let txn = second
        .storage()
        .begin(TxnOptions::default().read_only())
        .await
        .unwrap();
    let keys: Vec<Bytes> = (0..100).map(|i| Bytes::from(format!("k{i:03}"))).collect();
    let values = txn.get_many(keys).await.unwrap();
    for (i, value) in values.iter().enumerate() {
        assert_eq!(
            value.as_deref(),
            Some(format!("value {i}").as_bytes()),
            "k{i:03}"
        );
    }
}
