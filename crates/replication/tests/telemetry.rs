//! Replication's metrics and spans (ADR-0039 phase 3).
//!
//! A test recorder and subscriber are installed once for this binary;
//! there is one test, so nothing else moves the metrics meanwhile.

use std::collections::BTreeSet;
use std::time::Duration;

use bytes::Bytes;
use interlaken_replication::telemetry::METRICS;
use interlaken_replication::{MemoryNetwork, ReplicaConfig, ReplicaNode};
use interlaken_storage::{Storage, StorageOptions, TxnOptions};
use interlaken_telemetry::{install_test_recorder, install_test_subscriber};

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
/// Assert Raft's state, proposals, fresh reads and a snapshot transfer are
/// recorded, with their spans.
async fn replication_metrics_move() {
    let recorder = install_test_recorder();
    let subscriber = install_test_subscriber();
    let network = MemoryNetwork::new();
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let first = ReplicaNode::open(
        dirs[0].path().to_path_buf(),
        StorageOptions::default(),
        config(1),
        network.transport(1),
    )
    .await
    .unwrap();
    network.register(1, first.handler());
    first.initialize(&[1]).await.unwrap();
    while first.leader() != Some(1) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for i in 0..60 {
        let txn = first.storage().begin(TxnOptions::default()).await.unwrap();
        txn.put(Bytes::from(format!("k{i:03}")), Bytes::from("value"))
            .unwrap();
        txn.commit().await.unwrap();
    }
    while first.status().snapshot_index.is_none() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let node = [("node", "1")];
    assert_eq!(
        recorder.gauge("interlaken_raft_is_leader", &node),
        Some(1.0)
    );
    assert!(recorder.gauge("interlaken_raft_term", &node).unwrap() >= 1.0);
    assert!(recorder.counter("interlaken_raft_leader_changes_total", &node) >= 1);
    assert!(
        recorder
            .gauge("interlaken_raft_applied_index", &node)
            .unwrap()
            >= 60.0
    );
    let proposals = recorder.histogram("interlaken_raft_proposal_size", &[]);
    assert!(proposals.iter().sum::<f64>() >= 60.0);
    assert!(
        !recorder
            .histogram("interlaken_raft_proposal_duration_seconds", &[])
            .is_empty()
    );
    assert!(!subscriber.spans("interlaken.raft.propose").is_empty());
    // Read-write transactions take a fresh snapshot first.
    let reads = recorder.counter("interlaken_raft_reads_total", &[("kind", "lease")])
        + recorder.counter("interlaken_raft_reads_total", &[("kind", "read_index")]);
    assert!(reads >= 60);

    // A new replica catches up from a snapshot.
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
    assert!(second.status().snapshot_index.is_some());
    let snapshots = |direction: &str| {
        recorder.counter(
            "interlaken_raft_snapshots_total",
            &[("direction", direction), ("outcome", "ok")],
        )
    };
    assert_eq!(snapshots("sent"), 1);
    assert_eq!(snapshots("received"), 1);
    let bytes = |direction: &str| {
        recorder.counter(
            "interlaken_raft_snapshot_bytes_total",
            &[("direction", direction)],
        )
    };
    assert!(bytes("sent") > 512);
    assert_eq!(bytes("sent"), bytes("received"));
    assert_eq!(subscriber.spans("interlaken.raft.snapshot.send").len(), 1);
    assert_eq!(
        subscriber.spans("interlaken.raft.snapshot.install").len(),
        1
    );
    // The leader records how far the learner trails it.
    for _ in 0..100 {
        if recorder
            .gauge(
                "interlaken_raft_follower_lag_entries",
                &[("node", "1"), ("peer", "2")],
            )
            .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        recorder
            .gauge(
                "interlaken_raft_follower_lag_entries",
                &[("node", "1"), ("peer", "2")]
            )
            .is_some()
    );

    let declared: BTreeSet<String> = METRICS.iter().map(|m| m.name.to_owned()).collect();
    let undeclared: Vec<String> = recorder
        .names()
        .into_iter()
        .filter(|name| name.starts_with("interlaken_raft_") && !declared.contains(name))
        .collect();
    assert!(undeclared.is_empty(), "undeclared metrics: {undeclared:?}");
}
