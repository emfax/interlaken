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
