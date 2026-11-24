//! The transport's metrics (ADR-0039 phase 3).
//!
//! A test recorder is installed once for this binary; there is one test,
//! so nothing else moves the metrics meanwhile.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use bytes::Bytes;
use common::Pki;
use interlaken_net::telemetry::METRICS;
use interlaken_net::{Cluster, ClusterConfig, TlsConfig};
use interlaken_storage::{Storage, StorageOptions, TxnOptions};
use interlaken_telemetry::install_test_recorder;
use tempfile::TempDir;

/// Open node `id` in `dir` on an ephemeral localhost port, bootstrapping a
/// cluster if `bootstrap`.
async fn open(id: u64, dir: &TempDir, tls: TlsConfig, bootstrap: bool) -> Cluster {
    let mut config = ClusterConfig::new(id, "127.0.0.1:0".parse().unwrap(), tls);
    config.replica.heartbeat = Duration::from_millis(100);
    config.replica.election_min = Duration::from_millis(500);
    config.replica.election_max = Duration::from_millis(1000);
    config.replica.commit.timeout = Duration::from_secs(4);
    config.request_timeout = Duration::from_secs(2);
    config.bootstrap = bootstrap;
    Cluster::open(dir.path().to_path_buf(), StorageOptions::default(), config)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert calls between two members are counted per peer, kind and
/// outcome, with their bytes, and the requests answered too.
async fn transport_metrics_move() {
    let recorder = install_test_recorder();
    let pki = Pki::new();
    let dirs = [TempDir::new().unwrap(), TempDir::new().unwrap()];
    let first = open(1, &dirs[0], pki.node(), true).await;
    first.wait_for_leader(Duration::from_secs(5)).await.unwrap();
    let second = open(2, &dirs[1], pki.node(), false).await;
    first
        .add_member(2, &second.local_addr().to_string())
        .await
        .unwrap();
    // A commit on the follower is forwarded to the leader.
    let txn = second.storage().begin(TxnOptions::default()).await.unwrap();
    txn.put(Bytes::from_static(b"k"), Bytes::from_static(b"v"))
        .unwrap();
    txn.commit().await.unwrap();

    let appends = recorder.counter(
        "interlaken_net_requests_total",
        &[("peer", "2"), ("kind", "append"), ("outcome", "ok")],
    );
    assert!(appends > 0);
    let commits = recorder.counter(
        "interlaken_net_requests_total",
        &[("peer", "1"), ("kind", "commit"), ("outcome", "ok")],
    );
    assert_eq!(commits, 1);
    for (peer, direction) in [
        ("1", "sent"),
        ("1", "received"),
        ("2", "sent"),
        ("2", "received"),
    ] {
        assert!(
            recorder.counter(
                "interlaken_net_bytes_total",
                &[("peer", peer), ("direction", direction)]
            ) > 0,
            "{peer} {direction}"
        );
    }
    assert!(recorder.counter("interlaken_net_served_total", &[("kind", "append")]) > 0);
    assert!(
        recorder.counter(
            "interlaken_net_served_bytes_total",
            &[("direction", "received")]
        ) > 0
    );

    let declared: BTreeSet<String> = METRICS.iter().map(|m| m.name.to_owned()).collect();
    let undeclared: Vec<String> = recorder
        .names()
        .into_iter()
        .filter(|name| name.starts_with("interlaken_net_") && !declared.contains(name))
        .collect();
    assert!(undeclared.is_empty(), "undeclared metrics: {undeclared:?}");
    second.shutdown().await;
    first.shutdown().await;
}
