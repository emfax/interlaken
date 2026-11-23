//! Storage's side of replication: proposing, applying and phantom
//! proposals (ADR-0037 phase 2).

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use interlaken_storage::replica::{CommitFuture, CommitPath, Replica};
use interlaken_storage::{
    CommitRequest, Error, FjallStorage, Storage, StorageOptions, Timestamp, TxnOptions,
};

/// A commit path that hands each request to the test instead of committing it.
#[derive(Default)]
struct Captured {
    /// Requests submitted, in order.
    requests: Mutex<Vec<CommitRequest>>,
}

impl CommitPath for Captured {
    /// Keep the request and report that the commit didn't happen.
    fn submit(&self, request: CommitRequest) -> CommitFuture<'_> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async { Err(Error::ReplicationUnavailable("captured".into())) })
    }
}

/// Open a replicated database whose commits are captured.
fn open(dir: &std::path::Path) -> (FjallStorage, Replica, Arc<Captured>) {
    let captured = Arc::new(Captured::default());
    let path = Arc::clone(&captured);
    let mut replica = None;
    let storage = FjallStorage::open_replicated(dir, &StorageOptions::default(), |r| {
        replica = Some(r);
        Ok(path as Arc<dyn CommitPath>)
    })
    .unwrap();
    (storage, replica.unwrap(), captured)
}

/// Begin a transaction that writes `key` = `value` and capture its commit request.
async fn request(
    storage: &FjallStorage,
    captured: &Captured,
    key: &str,
    value: &str,
) -> CommitRequest {
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    txn.put(
        Bytes::copy_from_slice(key.as_bytes()),
        Bytes::copy_from_slice(value.as_bytes()),
    )
    .unwrap();
    assert!(txn.commit().await.is_err());
    captured.requests.lock().unwrap().pop().unwrap()
}

/// Return `key`'s value at the latest applied commit.
async fn get(storage: &FjallStorage, key: &str) -> Option<Bytes> {
    let txn = storage
        .begin(TxnOptions::default().read_only())
        .await
        .unwrap();
    txn.get_many(vec![Bytes::copy_from_slice(key.as_bytes())])
        .await
        .unwrap()[0]
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a proposed batch commits at consecutive timestamps when applied,
/// and a request conflicting with an outstanding proposal fails at once.
async fn proposals_apply_and_conflict_before_applying() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, replica, captured) = open(dir.path());
    let a = request(&storage, &captured, "a", "1").await;
    let b = request(&storage, &captured, "b", "2").await;
    let again = request(&storage, &captured, "a", "3").await;
    let mut proposer = replica.proposer();
    let (batch, outcomes) = proposer.propose(&[a, b]);
    assert_eq!(
        outcomes.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
        [Timestamp::from_raw(1), Timestamp::from_raw(2)]
    );
    // Validated against the outstanding proposal, before it applies.
    let (none, outcomes) = proposer.propose(&[again]);
    assert!(none.is_none());
    assert_eq!(outcomes[0].as_ref().unwrap_err().sqlstate(), "40001");
    replica.apply(batch, Vec::new()).await.unwrap();
    assert_eq!(replica.visible_ts(), Timestamp::from_raw(2));
    assert_eq!(get(&storage, "a").await.as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&storage, "b").await.as_deref(), Some(&b"2"[..]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a deposed leader's phantom proposal is forgotten when another
/// leader's entry applies in its place, and validation then sees the entry
/// that committed rather than the phantom.
async fn phantom_proposals_are_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, replica, captured) = open(dir.path());
    let other_dir = tempfile::tempdir().unwrap();
    let (other_storage, other, other_captured) = open(other_dir.path());

    // Requests from the initial snapshot, proposed later.
    let p_from_start = request(&storage, &captured, "p", "later").await;
    let q_from_start = request(&storage, &captured, "q", "later").await;

    // This replica leads and proposes a write to "p", which never commits.
    let phantom = request(&storage, &captured, "p", "phantom").await;
    let (_lost, outcomes) = replica.proposer().propose(&[phantom]);
    assert_eq!(*outcomes[0].as_ref().unwrap(), Timestamp::from_raw(1));

    // Another replica leads instead and commits a write to "q" at the same timestamp.
    let real = request(&other_storage, &other_captured, "q", "real").await;
    let (batch, _) = other.proposer().propose(&[real]);
    other.apply(batch.clone(), Vec::new()).await.unwrap();
    replica.apply(batch, Vec::new()).await.unwrap();
    assert_eq!(get(&storage, "q").await.as_deref(), Some(&b"real"[..]));
    assert_eq!(get(&storage, "p").await, None);

    // Leading again: "p" is free, since the phantom never committed, but "q" conflicts.
    let mut proposer = replica.proposer();
    let (batch, outcomes) = proposer.propose(&[p_from_start, q_from_start]);
    assert_eq!(*outcomes[0].as_ref().unwrap(), Timestamp::from_raw(2));
    assert_eq!(outcomes[1].as_ref().unwrap_err().sqlstate(), "40001");
    replica.apply(batch, Vec::new()).await.unwrap();
    assert_eq!(get(&storage, "p").await.as_deref(), Some(&b"later"[..]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a snapshot carries a replica's data, last commit and chosen
/// `meta` entries to another, which continues from it: old snapshots fail
/// validation there, new ones commit at the next timestamp.
async fn snapshots_carry_a_replica() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, replica, captured) = open(dir.path());
    for i in 0..50 {
        let r = request(&storage, &captured, &format!("k{i}"), &i.to_string()).await;
        let (batch, _) = replica.proposer().propose(&[r]);
        replica.apply(batch, Vec::new()).await.unwrap();
    }
    // Overwrite one key, so the snapshot holds two versions of it.
    let r = request(&storage, &captured, "k0", "new").await;
    let (batch, _) = replica.proposer().propose(&[r]);
    replica
        .apply(batch, vec![(b"raft_applied".to_vec(), b"51".to_vec())])
        .await
        .unwrap();
    let snapshot = replica
        .capture()
        .await
        .unwrap()
        .export(vec![b"raft_applied".to_vec(), b"absent".to_vec()])
        .await
        .unwrap();

    let other_dir = tempfile::tempdir().unwrap();
    let (other_storage, other, other_captured) = open(other_dir.path());
    let stale = request(&other_storage, &other_captured, "k1", "stale").await;
    other.store_snapshot(snapshot.clone()).await.unwrap();
    assert_eq!(
        other.stored_snapshot().await.unwrap(),
        Some(snapshot.clone())
    );
    other.import(snapshot).await.unwrap();
    assert!(!other.import_pending().await.unwrap());
    assert_eq!(other.visible_ts(), replica.visible_ts());
    assert_eq!(
        other.meta(b"raft_applied".to_vec()).await.unwrap(),
        Some(b"51".to_vec())
    );
    assert_eq!(other.meta(b"absent".to_vec()).await.unwrap(), None);
    assert_eq!(
        get(&other_storage, "k0").await.as_deref(),
        Some(&b"new"[..])
    );
    assert_eq!(
        get(&other_storage, "k49").await.as_deref(),
        Some(&b"49"[..])
    );
    // Only current data arrived: a request from before the snapshot can't be validated.
    let mut proposer = other.proposer();
    let (_, outcomes) = proposer.propose(&[stale]);
    assert_eq!(outcomes[0].as_ref().unwrap_err().sqlstate(), "40001");
    let fresh = request(&other_storage, &other_captured, "k1", "fresh").await;
    let (batch, outcomes) = proposer.propose(&[fresh]);
    assert_eq!(
        outcomes[0].as_ref().unwrap().into_raw(),
        replica.visible_ts().into_raw() + 1
    );
    other.apply(batch, Vec::new()).await.unwrap();
    assert_eq!(
        get(&other_storage, "k1").await.as_deref(),
        Some(&b"fresh"[..])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert an import a crash interrupted is reported after reopening, and
/// redoing it from the stored snapshot completes it.
async fn interrupted_imports_are_redone() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, replica, captured) = open(dir.path());
    let r = request(&storage, &captured, "k", "v").await;
    let (batch, _) = replica.proposer().propose(&[r]);
    replica.apply(batch, Vec::new()).await.unwrap();
    let snapshot = replica
        .capture()
        .await
        .unwrap()
        .export(Vec::new())
        .await
        .unwrap();

    let other_dir = tempfile::tempdir().unwrap();
    {
        let (_storage, other, _) = open(other_dir.path());
        other.store_snapshot(snapshot).await.unwrap();
        // What a crash part-way through an import leaves behind.
        other
            .set_meta(vec![(b"replica_importing".to_vec(), b"1".to_vec())], true)
            .await
            .unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (other_storage, other, _) = open(other_dir.path());
    assert!(other.import_pending().await.unwrap());
    let stored = other.stored_snapshot().await.unwrap().unwrap();
    other.import(stored).await.unwrap();
    assert!(!other.import_pending().await.unwrap());
    assert_eq!(get(&other_storage, "k").await.as_deref(), Some(&b"v"[..]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a damaged snapshot is refused before the replica's data is touched.
async fn damaged_snapshots_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, replica, captured) = open(dir.path());
    let r = request(&storage, &captured, "k", "v").await;
    let (batch, _) = replica.proposer().propose(&[r]);
    replica.apply(batch, Vec::new()).await.unwrap();
    let snapshot = replica
        .capture()
        .await
        .unwrap()
        .export(Vec::new())
        .await
        .unwrap();
    for len in 0..snapshot.len() {
        let error = replica.import(snapshot[..len].to_vec()).await.unwrap_err();
        assert_eq!(error.sqlstate(), "XX000", "prefix of {len}: {error}");
        assert!(!replica.import_pending().await.unwrap());
        assert_eq!(get(&storage, "k").await.as_deref(), Some(&b"v"[..]));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a transaction whose snapshot predates an installed snapshot fails
/// at its next operation, since the versions it read are gone.
async fn installs_end_older_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, replica, captured) = open(dir.path());
    for i in 0..3 {
        let r = request(&storage, &captured, &format!("k{i}"), "v").await;
        let (batch, _) = replica.proposer().propose(&[r]);
        replica.apply(batch, Vec::new()).await.unwrap();
    }
    let snapshot = replica
        .capture()
        .await
        .unwrap()
        .export(Vec::new())
        .await
        .unwrap();

    let other_dir = tempfile::tempdir().unwrap();
    let (other_storage, other, _) = open(other_dir.path());
    let old = other_storage
        .begin(TxnOptions::default().read_only())
        .await
        .unwrap();
    other.import(snapshot).await.unwrap();
    let error = old
        .get_many(vec![Bytes::from_static(b"k0")])
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), "25P04", "{error}");
    assert_eq!(get(&other_storage, "k2").await.as_deref(), Some(&b"v"[..]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a snapshot exported in small chunks concatenates to the whole
/// export, and chunks stored as they arrive import from storage.
async fn snapshots_stream_in_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, replica, captured) = open(dir.path());
    for i in 0..40 {
        let r = request(&storage, &captured, &format!("k{i:02}"), &"v".repeat(i)).await;
        let (batch, _) = replica.proposer().propose(&[r]);
        replica.apply(batch, Vec::new()).await.unwrap();
    }
    let whole = replica
        .capture()
        .await
        .unwrap()
        .export(Vec::new())
        .await
        .unwrap();
    let mut chunks = replica.capture().await.unwrap().chunks(Vec::new(), 64);
    let mut parts = Vec::new();
    while let Some(chunk) = chunks.next().await.unwrap() {
        parts.push(chunk);
    }
    assert!(parts.len() > 10, "{} chunks", parts.len());
    assert_eq!(parts.concat(), whole);

    let other_dir = tempfile::tempdir().unwrap();
    let (other_storage, other, _) = open(other_dir.path());
    let mut sink = other.begin_receiving().await.unwrap();
    for part in parts {
        sink.push(part).await.unwrap();
    }
    sink.finish().await.unwrap();
    other.import_stored().await.unwrap();
    assert_eq!(other.visible_ts(), replica.visible_ts());
    assert_eq!(
        get(&other_storage, "k39").await,
        Some(Bytes::from("v".repeat(39)))
    );
    assert_eq!(get(&other_storage, "k00").await, Some(Bytes::new()));
}
