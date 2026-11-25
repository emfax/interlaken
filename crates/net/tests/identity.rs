//! Peer identities, the grace period for expired certificates, and
//! rotation while running (ADR-0041).
//!
//! The tests run one at a time, so each can read the refusal counter's
//! growth during it.

#![allow(clippy::await_holding_lock)] // the lock only serialises tests

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::Pki;
use interlaken_net::{Cluster, ClusterConfig, MemberIdentity, TlsConfig, TlsMaterial};
use interlaken_storage::{Freshness, Storage, StorageOptions, TxnOptions};
use tempfile::TempDir;

/// Serialises the tests, which share the process's metrics recorder.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The refusal counter's name.
const REFUSED: &str = "interlaken_net_peer_refused_total";

/// Return how many peers have been refused for `reason`.
fn refusals(reason: &str) -> u64 {
    interlaken_telemetry::install_test_recorder().counter(REFUSED, &[("reason", reason)])
}

/// Take the lock that serialises the tests.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The URI prefix of this test's cluster.
const PREFIX: &str = "spiffe://test.example/cluster/c1/node/";

/// Return node `id`'s URI in cluster `cluster`.
fn uri(cluster: &str, id: u64) -> String {
    format!("spiffe://test.example/cluster/{cluster}/node/{id}")
}

/// Return a configuration from `material`, identifying members by URI,
/// with `grace` for expired certificates.
fn tls(material: TlsMaterial, grace: Duration) -> TlsConfig {
    TlsConfig::new(material.ca, material.cert, material.key)
        .with_verifier(Arc::new(MemberIdentity::new(PREFIX)))
        .with_expired_grace(grace)
}

/// Open node `id` in `dir`, bootstrapping a cluster if `bootstrap`.
async fn open(id: u64, dir: &TempDir, tls: TlsConfig, bootstrap: bool) -> Cluster {
    let mut config = ClusterConfig::new(id, "127.0.0.1:0".parse().unwrap(), tls);
    config.replica.heartbeat = Duration::from_millis(100);
    config.replica.election_min = Duration::from_millis(500);
    config.replica.election_max = Duration::from_millis(1000);
    config.replica.commit.timeout = Duration::from_secs(3);
    config.request_timeout = Duration::from_secs(2);
    config.max_connection_age = Duration::from_secs(1);
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

/// Form a cluster of three in `dirs`, each node's certificate naming its
/// URI, with `grace` for expired certificates.
async fn three(pki: &Pki, dirs: &[TempDir; 3], grace: Duration) -> Vec<Cluster> {
    let member = |id| tls(pki.member(&uri("c1", id), None, &pki.ca()), grace);
    let first = open(1, &dirs[0], member(1), true).await;
    first.wait_for_leader(Duration::from_secs(5)).await.unwrap();
    let second = open(2, &dirs[1], member(2), false).await;
    let third = open(3, &dirs[2], member(3), false).await;
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

/// Return the directories for three nodes.
fn dirs() -> [TempDir; 3] {
    [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ]
}

/// Assert writes on every node replicate to every node, under keys
/// starting with `prefix`.
async fn replicates(nodes: &[Cluster], prefix: &str) {
    for (i, node) in nodes.iter().enumerate() {
        put(node, &format!("{prefix}{i}"), "v").await.unwrap();
    }
    for node in nodes {
        for i in 0..nodes.len() {
            assert_eq!(
                get(node, &format!("{prefix}{i}")).await.as_deref(),
                Some("v")
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert members identified by URI replicate, and a node whose URI names
/// another cluster is refused though the same CA signed it.
async fn members_are_identified_by_uri() {
    let _serial = serial();
    let named = refusals("name");
    let pki = Pki::new();
    let dirs = dirs();
    let nodes = three(&pki, &dirs, Duration::ZERO).await;
    replicates(&nodes, "a").await;

    let other_dir = tempfile::tempdir().unwrap();
    let other = open(
        4,
        &other_dir,
        tls(pki.member(&uri("c2", 4), None, &pki.ca()), Duration::ZERO),
        true,
    )
    .await;
    let error = nodes[0]
        .add_member(4, &other.local_addr().to_string())
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), "57P03", "{error}");
    assert_eq!(nodes[0].status().voters, vec![1, 2, 3]);
    assert!(refusals("name") > named, "refused for its name");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a removed member is refused once its connections age out: its
/// writes stop committing, while the others carry on.
async fn removed_members_are_refused() {
    let _serial = serial();
    let pki = Pki::new();
    let dirs = dirs();
    let nodes = three(&pki, &dirs, Duration::ZERO).await;
    replicates(&nodes, "a").await;
    let leader = nodes[0]
        .wait_for_leader(Duration::from_secs(5))
        .await
        .unwrap();
    let removed = [1, 2, 3].into_iter().find(|&id| id != leader).unwrap();
    nodes[(leader - 1) as usize]
        .remove_member(removed)
        .await
        .unwrap();
    let outcast = &nodes[(removed - 1) as usize];
    let not_members = refusals("membership");
    let mut refused = false;
    for attempt in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if put(outcast, &format!("late{attempt}"), "v").await.is_err() {
            refused = true;
            break;
        }
    }
    assert!(refused, "the removed member kept committing");
    assert!(
        refusals("membership") > not_members,
        "refused as a non-member"
    );
    let staying: Vec<&Cluster> = nodes.iter().filter(|n| n.status().id != removed).collect();
    put(staying[0], "after", "v").await.unwrap();
    assert_eq!(get(staying[1], "after").await.as_deref(), Some("v"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a member whose certificate expired within the grace period keeps
/// replicating, and a non-member with such a certificate is refused.
async fn expired_members_within_the_grace_period() {
    let _serial = serial();
    let pki = Pki::new();
    let dirs = dirs();
    let week = Duration::from_secs(7 * 86_400);
    let nodes = three(&pki, &dirs, week).await;
    let refused_before = refusals("expired") + refusals("chain");
    nodes[1]
        .rotate_tls(&pki.member(&uri("c1", 2), Some(1), &pki.ca()))
        .unwrap();
    replicates(&nodes, "a").await;
    let expiry = interlaken_telemetry::install_test_recorder().gauge(
        "interlaken_net_certificate_expiry_seconds",
        &[("node", "2")],
    );
    assert!(
        expiry.is_some_and(|s| s < -80_000.0),
        "a day expired: {expiry:?}"
    );
    assert_eq!(
        refusals("expired") + refusals("chain"),
        refused_before,
        "the expired member was never refused"
    );

    let other_dir = tempfile::tempdir().unwrap();
    let late = open(
        4,
        &other_dir,
        tls(pki.member(&uri("c1", 4), Some(1), &pki.ca()), week),
        true,
    )
    .await;
    let error = nodes[0]
        .add_member(4, &late.local_addr().to_string())
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), "57P03", "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a member whose certificate expired beyond the grace period is
/// refused: its writes stop committing.
async fn expired_beyond_the_grace_period() {
    let _serial = serial();
    let pki = Pki::new();
    let dirs = dirs();
    let nodes = three(&pki, &dirs, Duration::from_secs(86_400)).await;
    replicates(&nodes, "a").await;
    let leader = nodes[0]
        .wait_for_leader(Duration::from_secs(5))
        .await
        .unwrap();
    let stale = [1, 2, 3].into_iter().find(|&id| id != leader).unwrap();
    let node = &nodes[(stale - 1) as usize];
    let expired = refusals("expired");
    node.rotate_tls(&pki.member(&uri("c1", stale), Some(3), &pki.ca()))
        .unwrap();
    let mut refused = false;
    for attempt in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if put(node, &format!("late{attempt}"), "v").await.is_err() {
            refused = true;
            break;
        }
    }
    assert!(refused, "a certificate three days expired kept working");
    assert!(refusals("expired") > expired, "refused as expired");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert certificates and the CA rotate on every member while running:
/// first to the new CA's certificates trusting both CAs, then trusting the
/// new CA only, with writes replicating throughout.
async fn rotation_while_running() {
    let _serial = serial();
    let old = Pki::new();
    let new = Pki::new();
    let dirs = dirs();
    let nodes = three(&old, &dirs, Duration::ZERO).await;
    replicates(&nodes, "a").await;
    let both = format!("{}{}", old.ca(), new.ca());
    for (i, node) in nodes.iter().enumerate() {
        let id = i as u64 + 1;
        node.rotate_tls(&new.member(&uri("c1", id), None, &both))
            .unwrap();
    }
    replicates(&nodes, "b").await;
    for (i, node) in nodes.iter().enumerate() {
        let id = i as u64 + 1;
        node.rotate_tls(&new.member(&uri("c1", id), None, &new.ca()))
            .unwrap();
    }
    // Wait out the old connections, which used the old certificates.
    tokio::time::sleep(Duration::from_secs(2)).await;
    replicates(&nodes, "c").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a rotation with a mismatched key or a certificate from an
/// untrusted CA is refused and changes nothing.
async fn bad_rotations_are_refused() {
    let _serial = serial();
    let pki = Pki::new();
    let other = Pki::new();
    let dirs = dirs();
    let nodes = three(&pki, &dirs, Duration::ZERO).await;
    let good = pki.member(&uri("c1", 1), None, &pki.ca());
    let mut mismatched = good.clone();
    mismatched.key = pki.member(&uri("c1", 1), None, &pki.ca()).key;
    assert!(nodes[0].rotate_tls(&mismatched).is_err());
    let untrusted = other.member(&uri("c1", 1), None, &pki.ca());
    assert!(nodes[0].rotate_tls(&untrusted).is_err());
    let empty = TlsMaterial {
        ca: Vec::new(),
        ..good
    };
    assert!(nodes[0].rotate_tls(&empty).is_err());
    replicates(&nodes, "a").await;
}
