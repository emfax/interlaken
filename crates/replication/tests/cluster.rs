//! Replicated clusters in one process, over the in-memory transport (ADR-0037 phase 2).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use common::Cluster;
use interlaken_storage::{Storage, TxnOptions};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert commits on the leader and on followers reach every replica, and a
/// committing node sees its own commit at once.
async fn commits_reach_every_replica() {
    let cluster = Cluster::start(3).await;
    let leader = cluster.leader().await;
    let follower = cluster.follower(leader);
    let on_leader = cluster.put(leader, "a", "1").await.unwrap();
    let on_follower = cluster.put(follower, "b", "2").await.unwrap();
    assert!(on_follower > on_leader);
    // Read-your-writes: no wait needed on the committing node.
    assert!(cluster.nodes[&follower].storage().visible_ts() >= on_follower);
    for &id in &cluster.ids {
        assert_eq!(
            cluster.get(id, "a", on_follower).await.as_deref(),
            Some("1")
        );
        assert_eq!(
            cluster.get(id, "b", on_follower).await.as_deref(),
            Some("2")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert transactions on different replicas that write the same key from
/// the same snapshot conflict, as on one node.
async fn conflicts_span_replicas() {
    let cluster = Cluster::start(3).await;
    let leader = cluster.leader().await;
    let follower = cluster.follower(leader);
    let base = cluster.put(leader, "k", "0").await.unwrap();
    cluster.get(follower, "k", base).await;
    let key = Bytes::from_static(b"k");
    let first = cluster.nodes[&leader]
        .storage()
        .begin(TxnOptions::default())
        .await
        .unwrap();
    let second = cluster.nodes[&follower]
        .storage()
        .begin(TxnOptions::default())
        .await
        .unwrap();
    first
        .put(key.clone(), Bytes::from_static(b"leader"))
        .unwrap();
    second.put(key, Bytes::from_static(b"follower")).unwrap();
    first.commit().await.unwrap();
    let error = second.commit().await.unwrap_err();
    assert_eq!(error.sqlstate(), "40001", "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert five replicas accept concurrent commits from every node and agree
/// on the result: each counter's value is the number of its increments
/// that committed.
async fn five_replicas_agree_under_concurrent_commits() {
    let cluster = Arc::new(Cluster::start(5).await);
    let mut tasks = Vec::new();
    for &id in &cluster.ids {
        let cluster = Arc::clone(&cluster);
        tasks.push(tokio::spawn(async move {
            let mut committed = BTreeMap::<String, u64>::new();
            for i in 0..40 {
                let key = format!("counter{}", i % 3);
                let storage = cluster.nodes[&id].storage();
                let txn = storage.begin(TxnOptions::default()).await.unwrap();
                let current = txn
                    .get_many(vec![Bytes::copy_from_slice(key.as_bytes())])
                    .await
                    .unwrap()[0]
                    .as_ref()
                    .map_or(0, |v| {
                        String::from_utf8(v.to_vec())
                            .unwrap()
                            .parse::<u64>()
                            .unwrap()
                    });
                txn.put(
                    Bytes::copy_from_slice(key.as_bytes()),
                    Bytes::from((current + 1).to_string()),
                )
                .unwrap();
                match txn.commit().await {
                    Ok(_) => *committed.entry(key).or_default() += 1,
                    Err(e) => assert_eq!(e.sqlstate(), "40001", "{e}"),
                }
            }
            committed
        }));
    }
    let mut expected = BTreeMap::<String, u64>::new();
    for task in tasks {
        for (key, n) in task.await.unwrap() {
            *expected.entry(key).or_default() += n;
        }
    }
    let last = cluster.put(1, "done", "1").await.unwrap();
    for &id in &cluster.ids {
        for (key, n) in &expected {
            assert_eq!(
                cluster.get(id, key, last).await,
                Some(n.to_string()),
                "node {id} {key}"
            );
        }
    }
    assert!(expected.values().sum::<u64>() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert the cluster elects a new leader when the leader crashes, keeps
/// committing, and the old leader catches up when it restarts.
async fn a_new_leader_takes_over_and_the_old_one_catches_up() {
    let mut cluster = Cluster::start(3).await;
    let leader = cluster.leader().await;
    let before = cluster.put(leader, "before", "1").await.unwrap();
    cluster.crash(leader).await;
    let successor = cluster.leader().await;
    assert_ne!(successor, leader);
    let after = cluster
        .put(cluster.follower(successor), "after", "2")
        .await
        .unwrap();
    assert!(after > before);
    cluster.boot(leader).await;
    assert_eq!(
        cluster.get(leader, "before", after).await.as_deref(),
        Some("1")
    );
    assert_eq!(
        cluster.get(leader, "after", after).await.as_deref(),
        Some("2")
    );
    // The restarted node can commit again, through whichever node leads.
    let again = cluster.put(leader, "again", "3").await.unwrap();
    for &id in &cluster.ids {
        assert_eq!(cluster.get(id, "again", again).await.as_deref(), Some("3"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a node cut off from the majority can't commit, failing with
/// 57P03 or an unknown outcome, while the majority keeps committing, and
/// that it converges once the partition heals.
async fn a_minority_cannot_commit() {
    let cluster = Cluster::start(3).await;
    let leader = cluster.leader().await;
    let isolated = cluster.follower(leader);
    cluster.network.partition(&[isolated], &cluster.ids);
    let error = cluster.put(isolated, "x", "lost").await.unwrap_err();
    assert!(matches!(error.sqlstate(), "57P03" | "08007"), "{error}");
    let majority = *cluster.ids.iter().find(|&&id| id != isolated).unwrap();
    let committed = cluster.put(majority, "y", "kept").await.unwrap();
    cluster.network.heal();
    assert_eq!(
        cluster.get(isolated, "y", committed).await.as_deref(),
        Some("kept")
    );
    let last = cluster.put(isolated, "z", "after").await.unwrap();
    for &id in &cluster.ids {
        assert_eq!(cluster.get(id, "x", last).await, None, "node {id}");
        assert_eq!(cluster.get(id, "z", last).await.as_deref(), Some("after"));
    }
}
