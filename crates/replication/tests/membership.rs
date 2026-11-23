//! Snapshots, log compaction and membership changes (ADR-0037 phase 4).

mod common;

use std::time::Duration;

use common::Cluster;
use interlaken_replication::ReplicaConfig;
use interlaken_storage::{Freshness, Timestamp};

/// Snapshot every 20 entries and keep 5, so replicas behind catch up from snapshots.
fn small_snapshots(config: &mut ReplicaConfig) {
    config.snapshot_every = 20;
    config.logs_kept = 5;
}

/// Commit `count` keys named after `prefix` through node `id`; return the
/// last commit. A commit whose outcome is unknown, or that found no leader,
/// is retried: writing the same value again is harmless.
async fn fill(cluster: &Cluster, id: u64, prefix: &str, count: u32) -> Timestamp {
    let mut last = Timestamp::ZERO;
    for i in 0..count {
        let key = format!("{prefix}{i}");
        last = loop {
            match cluster.put(id, &key, &i.to_string()).await {
                Ok(ts) => break ts,
                Err(e) if matches!(e.sqlstate(), "08007" | "57P03") => {}
                Err(e) => panic!("committing {key}: {e}"),
            }
        };
    }
    last
}

/// Assert node `id` holds the `count` keys `fill` wrote under `prefix`.
async fn assert_holds(cluster: &Cluster, id: u64, prefix: &str, count: u32, ts: Timestamp) {
    for i in 0..count {
        assert_eq!(
            cluster.get(id, &format!("{prefix}{i}"), ts).await,
            Some(i.to_string()),
            "node {id} lacks {prefix}{i}"
        );
    }
}

/// Wait until node `id` reports a snapshot.
async fn wait_for_snapshot(cluster: &Cluster, id: u64) {
    for _ in 0..500 {
        if cluster.nodes[&id].status().snapshot_index.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("node {id} never took a snapshot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a new node added as a learner after the log was compacted is
/// rebuilt from a snapshot, holds the leader's data, and can then vote.
async fn a_new_replica_is_built_from_a_snapshot() {
    let mut cluster = Cluster::start_with(3, small_snapshots).await;
    let leader = cluster.leader().await;
    let last = fill(&cluster, leader, "k", 100).await;
    wait_for_snapshot(&cluster, leader).await;
    let status = cluster.nodes[&leader].status();
    assert!(status.snapshot_index.unwrap() > 20, "{status:?}");

    cluster.add_node(4).await;
    cluster.nodes[&leader].add_learner(4).await.unwrap();
    assert_holds(&cluster, 4, "k", 100, last).await;
    assert!(
        cluster.nodes[&4].status().snapshot_index.is_some(),
        "node 4 started from a snapshot"
    );
    assert_eq!(cluster.nodes[&4].storage().visible_ts(), last);

    cluster.nodes[&leader]
        .set_voters(&[1, 2, 3, 4])
        .await
        .unwrap();
    let status = cluster.nodes[&cluster.leader().await].status();
    assert_eq!(status.voters, vec![1, 2, 3, 4]);
    assert!(status.learners.is_empty());
    let more = cluster.put(4, "after", "1").await.unwrap();
    for id in 1..=4 {
        assert_eq!(cluster.get(id, "after", more).await.as_deref(), Some("1"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a replica down while the log was compacted past it catches up
/// from a snapshot when it returns.
async fn a_replica_far_behind_catches_up_from_a_snapshot() {
    let mut cluster = Cluster::start_with(3, small_snapshots).await;
    let leader = cluster.leader().await;
    let behind = cluster.follower(leader);
    fill(&cluster, leader, "early", 10).await;
    cluster.crash(behind).await;
    let last = fill(&cluster, leader, "k", 100).await;
    wait_for_snapshot(&cluster, leader).await;
    cluster.boot(behind).await;
    assert_holds(&cluster, behind, "early", 10, last).await;
    assert_holds(&cluster, behind, "k", 100, last).await;
    let more = cluster.put(behind, "after", "1").await.unwrap();
    assert_eq!(
        cluster
            .read(behind, "after", Freshness::Local)
            .await
            .unwrap()
            .as_deref(),
        Some("1")
    );
    assert!(more > last);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a voter can be replaced, the leader included, while commits continue.
async fn the_leader_can_be_replaced() {
    let mut cluster = Cluster::start(3).await;
    let old = cluster.leader().await;
    let before = fill(&cluster, old, "k", 10).await;
    cluster.add_node(4).await;
    let remaining: Vec<u64> = (1..=4).filter(|&id| id != old).collect();
    cluster.nodes[&old].add_learner(4).await.unwrap();
    cluster.nodes[&old].set_voters(&remaining).await.unwrap();
    cluster.crash(old).await;
    cluster.ids.retain(|&id| id != old);
    let leader = cluster.leader().await;
    assert_ne!(leader, old);
    let status = cluster.nodes[&leader].status();
    assert_eq!(status.voters, remaining);
    let after = cluster.put(4, "after", "1").await.unwrap();
    assert!(after > before);
    for &id in &remaining {
        assert_holds(&cluster, id, "k", 10, after).await;
        assert_eq!(cluster.get(id, "after", after).await.as_deref(), Some("1"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert leadership can be handed to a chosen voter, from any node.
async fn leadership_can_be_transferred() {
    let cluster = Cluster::start(3).await;
    let leader = cluster.leader().await;
    let target = cluster.follower(leader);
    let asker = *cluster
        .ids
        .iter()
        .find(|&&id| id != leader && id != target)
        .unwrap();
    cluster.nodes[&asker]
        .transfer_leadership(target)
        .await
        .unwrap();
    for _ in 0..1000 {
        if cluster.leader().await == target {
            let status = cluster.nodes[&target].status();
            assert_eq!(status.role, "leader");
            cluster.put(asker, "k", "v").await.unwrap();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("leadership never moved to {target}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert the leader's status reports each follower's lag, and roles.
async fn status_reports_roles_and_lag() {
    let cluster = Cluster::start(3).await;
    let leader = cluster.leader().await;
    fill(&cluster, leader, "k", 5).await;
    for _ in 0..200 {
        let status = cluster.nodes[&leader].status();
        if status.lag.len() == 2 && status.lag.values().all(|lag| *lag == Some(0)) {
            assert_eq!(status.role, "leader");
            assert_eq!(status.voters, vec![1, 2, 3]);
            let follower = cluster.nodes[&cluster.follower(leader)].status();
            assert_eq!(follower.role, "follower");
            assert_eq!(follower.leader, Some(leader));
            assert!(follower.lag.is_empty());
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "followers never caught up: {:?}",
        cluster.nodes[&leader].status()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert snapshots are made on demand (ADR-0038): replicas pass many
/// snapshot thresholds and compact their logs without storing any snapshot
/// data, and a replica that installs one keeps no copy afterwards.
async fn snapshots_store_nothing() {
    let mut cluster = Cluster::start_with(3, small_snapshots).await;
    let leader = cluster.leader().await;
    let last = fill(&cluster, leader, "k", 120).await;
    for &id in &cluster.ids.clone() {
        cluster.get(id, "k119", last).await;
        wait_for_snapshot(&cluster, id).await;
        assert_eq!(
            cluster.nodes[&id].stored_snapshot_bytes().await.unwrap(),
            0,
            "node {id}"
        );
    }
    cluster.add_node(4).await;
    cluster.nodes[&leader].add_learner(4).await.unwrap();
    assert_holds(&cluster, 4, "k", 120, last).await;
    assert_eq!(cluster.nodes[&4].stored_snapshot_bytes().await.unwrap(), 0);
}
