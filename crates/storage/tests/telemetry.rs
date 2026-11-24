//! The storage layer's metrics and spans (ADR-0039 phase 1).
//!
//! A test recorder and subscriber are installed once for this binary. Their
//! values are process-wide, so each test holds [`LOCK`] and compares
//! readings taken before and after the work it checks.

// Each test runs on its own runtime, and the lock only orders the tests.
#![allow(clippy::await_holding_lock)]

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use interlaken_storage::telemetry::{METRICS, describe};
use interlaken_storage::{
    Durability, Error, FjallStorage, Isolation, Storage, StorageOptions, TxnOptions,
};
use interlaken_telemetry::{
    TestRecorder, TestSubscriber, install_test_recorder, install_test_subscriber,
};

/// Serialises the tests, whose metrics are process-wide.
static LOCK: Mutex<()> = Mutex::new(());

/// A marker that keys and values carry, which must never reach a label or span field.
const MARKER: &str = "SECRET-MARKER-7f3a";

/// Take the lock and return the installed recorder and subscriber.
fn setup() -> (
    MutexGuard<'static, ()>,
    &'static TestRecorder,
    &'static TestSubscriber,
) {
    let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    (guard, install_test_recorder(), install_test_subscriber())
}

/// Open a database with buffered commits and no background threads.
fn open() -> (tempfile::TempDir, FjallStorage) {
    let dir = tempfile::tempdir().unwrap();
    let options = StorageOptions {
        durability: Durability::Buffered,
        gc_interval: None,
        metrics_interval: None,
        ..StorageOptions::default()
    };
    let storage = FjallStorage::open(dir.path(), options).unwrap();
    (dir, storage)
}

/// Return `text` as bytes.
fn b(text: &str) -> Bytes {
    Bytes::copy_from_slice(text.as_bytes())
}

/// Commit one transaction writing `key` = `value`.
async fn put(storage: &FjallStorage, key: &str, value: &str) {
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    txn.put(b(key), b(value)).unwrap();
    txn.commit().await.unwrap();
}

/// Return the commit counter for `outcome` under serializable isolation.
fn commits(recorder: &TestRecorder, outcome: &str) -> u64 {
    recorder.counter(
        "interlaken_storage_commits_total",
        &[("isolation", "serializable"), ("outcome", outcome)],
    )
}

#[tokio::test]
/// Assert commits, conflicts, bytes and group sizes are recorded, and agree
/// with `CommitStats`.
async fn commit_metrics_move() {
    let (_guard, recorder, _) = setup();
    let (_dir, storage) = open();
    let stats_before = storage.commit_stats();
    let committed_before = commits(recorder, "committed");
    let conflicts_before = commits(recorder, "conflict");
    let bytes_before = recorder.counter("interlaken_storage_bytes_committed_total", &[]);
    let durations_before = recorder
        .histogram(
            "interlaken_storage_commit_duration_seconds",
            &[("durability", "buffered")],
        )
        .len();
    let groups_before = recorder
        .histogram("interlaken_storage_commit_group_size", &[])
        .len();

    put(&storage, "a", "1").await;
    put(&storage, "bb", "22").await;
    // Two serializable transactions read and write the same key; the
    // second to commit conflicts.
    let first = storage.begin(TxnOptions::default()).await.unwrap();
    let second = storage.begin(TxnOptions::default()).await.unwrap();
    for txn in [&first, &second] {
        txn.get_many(vec![b("a")]).await.unwrap();
        txn.put(b("a"), b("3")).unwrap();
    }
    first.commit().await.unwrap();
    assert!(matches!(
        second.commit().await,
        Err(Error::SerializationFailure(_))
    ));

    assert_eq!(commits(recorder, "committed") - committed_before, 3);
    assert_eq!(commits(recorder, "conflict") - conflicts_before, 1);
    // Keys and values: "a1", "bb22", "a3".
    assert_eq!(
        recorder.counter("interlaken_storage_bytes_committed_total", &[]) - bytes_before,
        8
    );
    let durations = recorder.histogram(
        "interlaken_storage_commit_duration_seconds",
        &[("durability", "buffered")],
    );
    assert_eq!(durations.len() - durations_before, 4);
    let groups = recorder.histogram("interlaken_storage_commit_group_size", &[]);
    assert_eq!(groups.len() - groups_before, 3);
    assert!(groups[groups_before..].iter().all(|&n| n == 1.0));

    let stats = storage.commit_stats().since(&stats_before);
    assert_eq!(stats.committed, 3);
    assert_eq!(stats.conflicts, 1);
    assert!(
        recorder
            .gauge(
                "interlaken_storage_sequencer_busy_seconds",
                &[("phase", "write")]
            )
            .unwrap()
            > 0.0
    );
}

#[tokio::test]
/// Assert other isolation levels are labelled as such.
async fn commits_are_labelled_by_isolation() {
    let (_guard, recorder, _) = setup();
    let (_dir, storage) = open();
    let labels = [("isolation", "read_committed"), ("outcome", "committed")];
    let before = recorder.counter("interlaken_storage_commits_total", &labels);
    let txn = storage
        .begin(TxnOptions {
            isolation: Isolation::ReadCommitted,
            ..TxnOptions::default()
        })
        .await
        .unwrap();
    txn.put(b("k"), b("v")).unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        recorder.counter("interlaken_storage_commits_total", &labels) - before,
        1
    );
}

#[tokio::test]
/// Assert a garbage collection pass is recorded, and agrees with `GcStats`.
async fn gc_metrics_move() {
    let (_guard, recorder, subscriber) = setup();
    let (_dir, storage) = open();
    for value in ["1", "2", "3"] {
        put(&storage, "k", value).await;
    }
    let examined = |r: &TestRecorder| {
        r.counter(
            "interlaken_storage_gc_versions_total",
            &[("outcome", "examined")],
        )
    };
    let removed = |r: &TestRecorder| {
        r.counter(
            "interlaken_storage_gc_versions_total",
            &[("outcome", "removed")],
        )
    };
    let passes = |r: &TestRecorder| {
        r.counter(
            "interlaken_storage_gc_passes_total",
            &[("outcome", "completed")],
        )
    };
    let (examined_before, removed_before, passes_before) =
        (examined(recorder), removed(recorder), passes(recorder));
    let spans_before = subscriber.spans("interlaken.gc.pass").len();

    let report = storage.run_gc().await.unwrap();
    assert_eq!(report.removed, 2);

    assert_eq!(examined(recorder) - examined_before, report.examined);
    assert_eq!(removed(recorder) - removed_before, report.removed);
    assert_eq!(passes(recorder) - passes_before, 1);
    let gc = storage.metrics().await.unwrap().gc;
    assert_eq!(gc.versions_removed, report.removed);
    let spans = subscriber.spans("interlaken.gc.pass");
    assert_eq!(spans.len() - spans_before, 1);
    assert_eq!(spans.last().unwrap().field("removed"), Some("2"));
}

#[tokio::test]
/// Assert a sample records sizes and transaction counts.
async fn sampler_records_gauges() {
    let (_guard, recorder, _) = setup();
    let (_dir, storage) = open();
    put(&storage, "k", "v").await;
    let open_txn = storage.begin(TxnOptions::default()).await.unwrap();
    storage.record_metrics().await.unwrap();
    assert!(
        recorder
            .gauge("interlaken_storage_disk_bytes", &[("kind", "journal")])
            .unwrap()
            > 0.0
    );
    assert_eq!(
        recorder.gauge("interlaken_storage_active_transactions", &[]),
        Some(1.0)
    );
    assert!(
        recorder
            .gauge("interlaken_storage_oldest_snapshot_age_seconds", &[])
            .unwrap()
            >= 0.0
    );
    assert!(
        recorder
            .gauge("interlaken_storage_memtable_bytes", &[])
            .unwrap()
            > 0.0
    );
    drop(open_txn);
    storage.record_metrics().await.unwrap();
    assert_eq!(
        recorder.gauge("interlaken_storage_active_transactions", &[]),
        Some(0.0)
    );
    if cfg!(target_os = "linux") {
        assert!(recorder.counter("interlaken_storage_bytes_written_total", &[]) > 0);
    }
}

#[tokio::test]
/// Assert a commit records its span with isolation, durability and outcome.
async fn commit_span_is_recorded() {
    let (_guard, _, subscriber) = setup();
    let (_dir, storage) = open();
    let before = subscriber.spans("interlaken.commit").len();
    put(&storage, "k", "v").await;
    let spans = subscriber.spans("interlaken.commit");
    assert_eq!(spans.len() - before, 1);
    let span = spans.last().unwrap();
    assert_eq!(span.field("isolation"), Some("serializable"));
    assert_eq!(span.field("durability"), Some("buffered"));
    assert_eq!(span.field("outcome"), Some("committed"));
    let groups = subscriber.spans("interlaken.storage.commit_group");
    assert_eq!(groups.last().unwrap().field("bytes"), Some("2"));
}

#[tokio::test]
/// Assert keys and values never reach a metric label or span field.
async fn no_data_in_labels_or_spans() {
    let (_guard, recorder, subscriber) = setup();
    let (_dir, storage) = open();
    put(&storage, MARKER, MARKER).await;
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    txn.get_many(vec![b(MARKER)]).await.unwrap();
    txn.put(b(MARKER), b(MARKER)).unwrap();
    txn.commit().await.unwrap();
    storage.run_gc().await.unwrap();
    storage.record_metrics().await.unwrap();
    let leaked: Vec<String> = recorder
        .labels()
        .into_iter()
        .chain(subscriber.field_values())
        .filter(|value| value.contains(MARKER))
        .collect();
    assert!(leaked.is_empty(), "data reached telemetry: {leaked:?}");
}

#[tokio::test]
/// Assert every storage metric recorded is declared, and describing the
/// declarations reaches the recorder.
async fn recorded_metrics_are_declared() {
    let (_guard, recorder, _) = setup();
    let (_dir, storage) = open();
    put(&storage, "k", "v").await;
    storage.run_gc().await.unwrap();
    storage.record_metrics().await.unwrap();
    let declared: BTreeSet<String> = METRICS.iter().map(|m| m.name.to_owned()).collect();
    let recorded: BTreeSet<String> = recorder
        .names()
        .into_iter()
        .filter(|name| name.starts_with("interlaken_storage_"))
        .collect();
    let undeclared: Vec<&String> = recorded.difference(&declared).collect();
    assert!(undeclared.is_empty(), "undeclared metrics: {undeclared:?}");
    describe(METRICS);
    assert!(declared.is_subset(&recorder.described()));
    // Every declared label appears only on metrics that declare it.
    for metric in METRICS {
        for label in recorder.labels() {
            if let Some(rest) = label.strip_prefix(&format!("{}{{", metric.name)) {
                let key = rest.split('=').next().unwrap();
                assert!(
                    metric.labels.contains(&key),
                    "{} carries undeclared label {key}",
                    metric.name
                );
            }
        }
    }
}
