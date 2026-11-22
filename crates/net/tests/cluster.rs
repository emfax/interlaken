//! Cluster members over gRPC with mutual TLS, in one process on localhost
//! (ADR-0037 phase 5).

mod common;

use std::time::Duration;

use bytes::Bytes;
use common::Pki;
use interlaken_net::{Cluster, ClusterConfig, TlsConfig};
use interlaken_storage::{FjallStorage, Freshness, Storage, StorageOptions, TxnOptions};
use tempfile::TempDir;

/// Return settings for node `id` on an ephemeral localhost port.
fn config(id: u64, tls: TlsConfig) -> ClusterConfig {
    let mut config = ClusterConfig::new(id, "127.0.0.1:0".parse().unwrap(), tls);
    config.replica.heartbeat = Duration::from_millis(100);
    config.replica.election_min = Duration::from_millis(500);
    config.replica.election_max = Duration::from_millis(1000);
    config.replica.commit.timeout = Duration::from_secs(4);
    config.request_timeout = Duration::from_secs(2);
    config
}

/// Open node `id` in `dir`, bootstrapping a cluster if `bootstrap`.
async fn open(id: u64, dir: &TempDir, tls: TlsConfig, bootstrap: bool) -> Cluster {
    let mut config = config(id, tls);
    config.bootstrap = bootstrap;
    Cluster::open(dir.path().to_path_buf(), StorageOptions::default(), config)
        .await
        .unwrap()
}

/// Put `key` = `value` in one transaction on `cluster`.
async fn put(cluster: &Cluster, key: &str, value: &str) -> interlaken_storage::Result<()> {
    let txn = cluster.storage().begin(TxnOptions::default()).await?;
    txn.put(
        Bytes::copy_from_slice(key.as_bytes()),
        Bytes::copy_from_slice(value.as_bytes()),
    )?;
    txn.commit().await.map(drop)
}

/// Read `key` on `cluster` with a fresh snapshot.
async fn get(cluster: &Cluster, key: &str) -> Option<String> {
    let txn = cluster
        .storage()
        .begin(
            TxnOptions::default()
                .read_only()
                .freshness(Freshness::Fresh),
        )
        .await
        .unwrap();
    txn.get_many(vec![Bytes::copy_from_slice(key.as_bytes())])
        .await
        .unwrap()[0]
        .as_ref()
        .map(|v| String::from_utf8(v.to_vec()).unwrap())
}

/// Form a cluster of three: node 1 bootstraps and adds the others.
async fn three(pki: &Pki, dirs: &[TempDir; 3]) -> Vec<Cluster> {
    let first = open(1, &dirs[0], pki.node(), true).await;
    assert_eq!(
        first.wait_for_leader(Duration::from_secs(5)).await.unwrap(),
        1
    );
    let second = open(2, &dirs[1], pki.node(), false).await;
    let third = open(3, &dirs[2], pki.node(), false).await;
    first
        .add_member(2, &second.local_addr().to_string())
        .await
        .unwrap();
    first
        .add_member(3, &third.local_addr().to_string())
        .await
        .unwrap();
    vec![first, second, third]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert three members over gRPC replicate commits made on any of them,
/// report their roles, and count their traffic.
async fn members_replicate_over_grpc() {
    let pki = Pki::new();
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let nodes = three(&pki, &dirs).await;
    for (i, node) in nodes.iter().enumerate() {
        put(node, &format!("k{i}"), &i.to_string()).await.unwrap();
    }
    for node in &nodes {
        for i in 0..3 {
            assert_eq!(get(node, &format!("k{i}")).await, Some(i.to_string()));
        }
    }
    let status = nodes[0].status();
    assert_eq!(status.role, "leader");
    assert_eq!(status.voters, vec![1, 2, 3]);
    let traffic = nodes[0].net_metrics();
    assert!(
        traffic.peers.get(&2).is_some_and(|p| p.sent > 0),
        "{traffic:?}"
    );
    assert!(nodes[1].net_metrics().served > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a cluster bootstrapped from an existing single-node database
/// gives joining members its data, which no log entry carries.
async fn a_cluster_bootstraps_from_an_existing_database() {
    let pki = Pki::new();
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    {
        let storage = FjallStorage::open(dirs[0].path(), StorageOptions::default()).unwrap();
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        txn.put(Bytes::from_static(b"old"), Bytes::from_static(b"data"))
            .unwrap();
        txn.commit().await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let nodes = three(&pki, &dirs).await;
    put(&nodes[2], "new", "data").await.unwrap();
    for node in &nodes {
        assert_eq!(get(node, "old").await.as_deref(), Some("data"));
        assert_eq!(get(node, "new").await.as_deref(), Some("data"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert the cluster survives its leader stopping, and the restarted
/// member rejoins from its directory and address.
async fn members_survive_the_leader_stopping() {
    let pki = Pki::new();
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let mut nodes = three(&pki, &dirs).await;
    put(&nodes[1], "before", "1").await.unwrap();
    let old = nodes.remove(0);
    let old_addr = old.local_addr();
    let tls = pki.node();
    old.shutdown().await;
    drop(old);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let leader = nodes[0]
        .wait_for_leader(Duration::from_secs(10))
        .await
        .unwrap();
    for _ in 0..100 {
        if nodes[0].node().leader().is_some_and(|l| l != 1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(leader != 1 || nodes[0].node().leader() != Some(1));
    put(&nodes[0], "after", "2").await.unwrap();
    let mut config = config(1, tls);
    config.listen = old_addr;
    let back = Cluster::open(
        dirs[0].path().to_path_buf(),
        StorageOptions::default(),
        config,
    )
    .await
    .unwrap();
    assert_eq!(get(&back, "before").await.as_deref(), Some("1"));
    assert_eq!(get(&back, "after").await.as_deref(), Some("2"));
}

/// Assert a node with `tls` and a cluster whose CA is `pki`'s refuse each
/// other in both directions: the member can't add the node, and the node,
/// forming its own cluster, can't add the member. Neither answers a request.
async fn refused(pki: &Pki, tls: TlsConfig) {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let member = open(1, &dirs[0], pki.node(), true).await;
    member
        .wait_for_leader(Duration::from_secs(5))
        .await
        .unwrap();
    let rogue = open(2, &dirs[1], tls, true).await;
    rogue.wait_for_leader(Duration::from_secs(5)).await.unwrap();
    let error = member
        .add_member(2, &rogue.local_addr().to_string())
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), "57P03", "{error}");
    let error = rogue
        .add_member(1, &member.local_addr().to_string())
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), "57P03", "{error}");
    assert!(
        member
            .net_metrics()
            .peers
            .get(&2)
            .is_some_and(|p| p.failed > 0)
    );
    assert!(
        rogue
            .net_metrics()
            .peers
            .get(&1)
            .is_some_and(|p| p.failed > 0)
    );
    assert_eq!(rogue.net_metrics().served, 0, "the rogue answered nothing");
    assert_eq!(
        member.net_metrics().served,
        0,
        "the member answered nothing"
    );
    assert_eq!(member.status().voters, vec![1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a node whose certificate another CA signed is refused both ways.
async fn unknown_certificates_are_refused() {
    let pki = Pki::new();
    let other = Pki::new();
    refused(&pki, pki.node_signed_by(&other)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a node whose certificate has expired is refused both ways.
async fn expired_certificates_are_refused() {
    let pki = Pki::new();
    refused(&pki, pki.expired_node()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a measurement, not a test"]
/// Print commit throughput over gRPC from the leader and a follower.
async fn measure_commits_over_grpc() {
    let pki = Pki::new();
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let nodes = std::sync::Arc::new(three(&pki, &dirs).await);
    for (name, index) in [("leader", 0), ("follower", 1)] {
        for clients in [1usize, 16, 64] {
            let per = 2000 / clients;
            let start = std::time::Instant::now();
            let tasks: Vec<_> = (0..clients)
                .map(|c| {
                    let nodes = std::sync::Arc::clone(&nodes);
                    tokio::spawn(async move {
                        for i in 0..per {
                            put(&nodes[index], &format!("{name}{c}/{i}"), "v")
                                .await
                                .unwrap();
                        }
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }
            let rate = (per * clients) as f64 / start.elapsed().as_secs_f64();
            eprintln!("{name}, {clients} clients: {rate:.0} commits/s");
        }
    }
}
