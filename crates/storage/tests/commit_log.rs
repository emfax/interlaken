//! The commit log path (ADR-0037 phase 1): commits are appended to a log
//! and applied separately, and a database can move between this path and
//! the single-node sequencer from one open to the next.

use bytes::Bytes;
use interlaken_storage::{
    Durability, FjallStorage, Storage, StorageOptions, Timestamp, TxnOptions,
};

/// Return options for the given commit path, with buffered durability and no GC.
fn options(commit_log: bool) -> StorageOptions {
    StorageOptions {
        durability: Durability::Buffered,
        gc_interval: None,
        commit_log,
        ..StorageOptions::default()
    }
}

/// Commit `count` transactions concurrently, each writing `prefix-i = i`.
async fn commit_many(storage: &FjallStorage, prefix: &str, count: usize) {
    let mut tasks = Vec::new();
    for i in 0..count {
        let storage = storage.clone();
        let key = format!("{prefix}-{i:04}");
        tasks.push(tokio::spawn(async move {
            let txn = storage.begin(TxnOptions::default()).await.unwrap();
            txn.put(Bytes::from(key), Bytes::from(i.to_string()))
                .unwrap();
            txn.commit().await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}

/// Read the keys starting `prefix`.
async fn count(storage: &FjallStorage, prefix: &str, n: usize) -> usize {
    let txn = storage
        .begin(TxnOptions::default().read_only())
        .await
        .unwrap();
    let keys: Vec<Bytes> = (0..n)
        .map(|i| Bytes::from(format!("{prefix}-{i:04}")))
        .collect();
    txn.get_many(keys).await.unwrap().iter().flatten().count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert concurrent commits through the log are all visible, keep
/// consecutive timestamps, and survive closing and reopening on either
/// path, with timestamps continuing after the last.
async fn commits_survive_switching_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut last = Timestamp::ZERO;
    // Single-node, then log, then single-node, then log again; each open
    // sees everything committed before it, more than one compaction's worth.
    for (round, commit_log) in [false, true, false, true].into_iter().enumerate() {
        let storage = FjallStorage::open(dir.path(), options(commit_log)).unwrap();
        assert_eq!(
            storage.visible_ts(),
            last,
            "round {round} reopens at the last commit"
        );
        commit_many(&storage, &format!("r{round}"), 300).await;
        assert_eq!(
            storage.visible_ts().into_raw(),
            last.into_raw() + 300,
            "round {round}: one timestamp per commit"
        );
        for earlier in 0..=round {
            assert_eq!(count(&storage, &format!("r{earlier}"), 300).await, 300);
        }
        last = storage.visible_ts();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert conflicting transactions are still validated through the log:
/// of two concurrent increments of one key, one fails with a conflict.
async fn conflicts_are_detected_through_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let storage = FjallStorage::open(dir.path(), options(true)).unwrap();
    let setup = storage.begin(TxnOptions::default()).await.unwrap();
    setup
        .put(Bytes::from_static(b"n"), Bytes::from_static(b"0"))
        .unwrap();
    setup.commit().await.unwrap();
    let a = storage.begin(TxnOptions::default()).await.unwrap();
    let b = storage.begin(TxnOptions::default()).await.unwrap();
    a.put(Bytes::from_static(b"n"), Bytes::from_static(b"1"))
        .unwrap();
    b.put(Bytes::from_static(b"n"), Bytes::from_static(b"2"))
        .unwrap();
    a.commit().await.unwrap();
    let err = b.commit().await.unwrap_err();
    assert_eq!(err.sqlstate(), "40001", "{err}");
}
