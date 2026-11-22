//! Fresh and local snapshots, and history for follower transactions
//! (ADR-0037 phase 3).

mod common;

use std::time::Duration;

use bytes::Bytes;
use common::Cluster;
use interlaken_replication::ReplicaConfig;
use interlaken_storage::{Freshness, Storage, TxnOptions};

/// Turn lease reads off, so every read index takes a round of heartbeats.
fn no_lease(config: &mut ReplicaConfig) {
    config.commit.lease_reads = false;
}

/// Keep no history beyond what local snapshots need.
fn no_retention(config: &mut ReplicaConfig) {
    config.history_retention = Duration::ZERO;
}

/// Assert a fresh read on each follower sees a commit acknowledged
/// elsewhere just before, without waiting.
async fn fresh_reads_see_acknowledged_commits(cluster: Cluster) {
    let leader = cluster.leader().await;
    for round in 0..20 {
        let value = round.to_string();
        let writer = cluster.ids[round % cluster.ids.len()];
        cluster.put(writer, "k", &value).await.unwrap();
        for &reader in &cluster.ids {
            assert_eq!(
                cluster.read(reader, "k", Freshness::Fresh).await.unwrap(),
                Some(value.clone()),
                "round {round}: node {reader} after a commit on {writer} (leader {leader})"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert fresh reads are linearizable with the leader's lease.
async fn fresh_reads_with_leases() {
    fresh_reads_see_acknowledged_commits(Cluster::start(3).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert fresh reads are linearizable with heartbeat rounds instead of leases.
async fn fresh_reads_without_leases() {
    fresh_reads_see_acknowledged_commits(Cluster::start_with(3, no_lease).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a node cut off from the majority serves stale local reads but no
/// fresh ones, catches up once healed, and reads its own commits locally.
async fn local_reads_lag_and_catch_up() {
    let cluster = Cluster::start(3).await;
    let leader = cluster.leader().await;
    let isolated = cluster.follower(leader);
    cluster.put(leader, "k", "old").await.unwrap();
    while cluster.read(isolated, "k", Freshness::Local).await.unwrap() != Some("old".into()) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cluster.network.partition(&[isolated], &cluster.ids);
    cluster.put(leader, "k", "new").await.unwrap();
    assert_eq!(
        cluster
            .read(isolated, "k", Freshness::Local)
            .await
            .unwrap()
            .as_deref(),
        Some("old"),
        "a local read is stale while cut off"
    );
    let error = cluster
        .read(isolated, "k", Freshness::Fresh)
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), "57P03", "{error}");
    cluster.network.heal();
    let healed = tokio::time::Instant::now();
    while cluster.read(isolated, "k", Freshness::Local).await.unwrap() != Some("new".into()) {
        assert!(
            healed.elapsed() < Duration::from_secs(5),
            "local reads never caught up"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cluster.put(isolated, "mine", "1").await.unwrap();
    assert_eq!(
        cluster
            .read(isolated, "mine", Freshness::Local)
            .await
            .unwrap()
            .as_deref(),
        Some("1"),
        "a local read sees this node's own commit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a leader cut off from the majority, once replaced, refuses fresh
/// reads rather than serve data older than the new leader's commits.
async fn a_deposed_leader_refuses_fresh_reads() {
    let cluster = Cluster::start(3).await;
    let old = cluster.leader().await;
    cluster.put(old, "k", "before").await.unwrap();
    cluster.network.partition(&[old], &cluster.ids);
    let majority: Vec<u64> = cluster
        .ids
        .iter()
        .copied()
        .filter(|&id| id != old)
        .collect();
    let mut new = None;
    for _ in 0..400 {
        let leaders: Vec<_> = majority
            .iter()
            .map(|id| cluster.nodes[id].leader())
            .collect();
        if let Some(Some(l)) = leaders.first()
            && *l != old
            && leaders.iter().all(|x| x == &Some(*l))
        {
            new = Some(*l);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let new = new.expect("the majority elects a new leader");
    cluster.put(new, "k", "after").await.unwrap();
    match cluster.read(old, "k", Freshness::Fresh).await {
        Err(error) => assert_eq!(error.sqlstate(), "57P03", "{error}"),
        Ok(value) => panic!("the deposed leader served a fresh read of {value:?}"),
    }
}

/// Begin a transaction on node `id` that writes `key`, commit elsewhere
/// twice meanwhile, and return the transaction's commit result's SQLSTATE.
async fn commit_after_others(cluster: &Cluster, id: u64, other: u64) -> Option<&'static str> {
    let txn = cluster.nodes[&id]
        .storage()
        .begin(TxnOptions::default())
        .await
        .unwrap();
    txn.put(Bytes::from_static(b"mine"), Bytes::from_static(b"1"))
        .unwrap();
    cluster.put(other, "a", "1").await.unwrap();
    cluster.put(other, "b", "1").await.unwrap();
    txn.commit().await.err().map(|e| e.sqlstate())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert the leader keeps enough history to validate a follower's
/// transaction whose snapshot predates later commits, and that without
/// retention it can't, failing safely with `40001`.
async fn history_outlives_follower_snapshots() {
    let cluster = Cluster::start(3).await;
    let leader = cluster.leader().await;
    let follower = cluster.follower(leader);
    assert_eq!(commit_after_others(&cluster, follower, leader).await, None);

    let cluster = Cluster::start_with(3, no_retention).await;
    let leader = cluster.leader().await;
    let follower = cluster.follower(leader);
    assert_eq!(
        commit_after_others(&cluster, follower, leader).await,
        Some("40001"),
        "without retention the leader can't validate the old snapshot"
    );
}

/// Return the mean latency of `n` sequential read-only begins on node `id`,
/// and the rate of `n` begins from `clients` concurrent tasks.
async fn begin_cost(
    cluster: &Cluster,
    id: u64,
    freshness: Freshness,
    n: u32,
    clients: u32,
) -> (Duration, f64) {
    let storage = cluster.nodes[&id].storage().clone();
    let options = TxnOptions::default().read_only().freshness(freshness);
    let start = std::time::Instant::now();
    for _ in 0..n {
        drop(storage.begin(options).await.unwrap());
    }
    let sequential = start.elapsed() / n;
    let start = std::time::Instant::now();
    let tasks: Vec<_> = (0..clients)
        .map(|_| {
            let storage = storage.clone();
            tokio::spawn(async move {
                for _ in 0..n / clients {
                    drop(storage.begin(options).await.unwrap());
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let rate = f64::from(n / clients * clients) / start.elapsed().as_secs_f64();
    (sequential, rate)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a measurement, not a test"]
/// Print what a fresh snapshot costs on the leader and a follower, with
/// and without leases, against a local one.
async fn measure_fresh_snapshots() {
    for (name, configure) in [
        ("lease", None),
        ("no lease", Some(no_lease as fn(&mut ReplicaConfig))),
    ] {
        let cluster = match configure {
            Some(configure) => Cluster::start_with(3, configure).await,
            None => Cluster::start(3).await,
        };
        let leader = cluster.leader().await;
        let follower = cluster.follower(leader);
        for (role, id) in [("leader", leader), ("follower", follower)] {
            for freshness in [Freshness::Fresh, Freshness::Local] {
                let (latency, rate) = begin_cost(&cluster, id, freshness, 20_000, 64).await;
                eprintln!(
                    "{name}, {role}, {freshness:?}: {latency:?} each alone, {rate:.0}/s from 64 clients"
                );
            }
        }
    }
}
