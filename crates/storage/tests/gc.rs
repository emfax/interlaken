//! Version GC, transaction limits and metrics (ADR-0008, ADR-0004, milestone M5).

use std::time::Duration;

use bytes::Bytes;
use interlaken_storage::{
    Durability, Engine, Error, FjallStorage, Isolation, Key, KeyRange, ScanRequest, Storage,
    StorageOptions, TxnOptions,
};

/// Storage options for tests: buffered, no background GC unless asked.
fn options() -> StorageOptions {
    StorageOptions {
        durability: Durability::Buffered,
        gc_interval: None,
        gc_chunk_versions: 7,
        ..StorageOptions::default()
    }
}

/// Encode key `i`.
fn key(i: u64) -> Key {
    Bytes::from(format!("k{i:04}"))
}

/// Encode an integer value.
fn value(v: i64) -> Bytes {
    Bytes::from(v.to_be_bytes().to_vec())
}

/// Decode an integer value.
fn int(bytes: &[u8]) -> i64 {
    i64::from_be_bytes(bytes.try_into().unwrap())
}

/// Commit `key(i) = v` for every `i` in `keys`.
async fn write_all(storage: &FjallStorage, keys: std::ops::Range<u64>, v: i64) {
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    for i in keys {
        txn.put(key(i), value(v)).unwrap();
    }
    txn.commit().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert space stays bounded under sustained concurrent updates (ADR-0009 phase 5).
///
/// Each round, workers update every key many times while background GC
/// runs; after the round a pass with no open transactions must leave
/// exactly one version per live key, however many versions were written.
async fn space_stays_bounded_under_sustained_updates() {
    const KEYS: u64 = 50;
    let dir = tempfile::tempdir().unwrap();
    let storage = FjallStorage::open(
        dir.path(),
        StorageOptions {
            gc_interval: Some(Duration::from_millis(5)),
            ..options()
        },
    )
    .unwrap();
    write_all(&storage, 0..KEYS, 0).await;

    let mut written = 0u64;
    for round in 0..15 {
        let tasks: Vec<_> = (0..8u64)
            .map(|worker| {
                let storage = storage.clone();
                tokio::spawn(async move {
                    let mut commits = 0;
                    for n in 0..40u64 {
                        let txn = storage
                            .begin(TxnOptions::new(Isolation::ReadCommitted))
                            .await
                            .unwrap();
                        let i = (worker * 7 + n * 3) % KEYS;
                        txn.put(key(i), value(round)).unwrap();
                        if n % 10 == 0 {
                            // Some keys come and go.
                            txn.delete(key(KEYS + worker)).unwrap();
                        } else {
                            txn.put(key(KEYS + worker), value(1)).unwrap();
                        }
                        if txn.commit().await.is_ok() {
                            commits += 2;
                        }
                    }
                    commits
                })
            })
            .collect();
        for task in tasks {
            written += task.await.unwrap();
        }
        let report = storage.run_gc().await.unwrap();
        let live = {
            let txn = storage
                .begin(TxnOptions::default().read_only())
                .await
                .unwrap();
            txn.scan(ScanRequest::new(KeyRange::all()))
                .await
                .unwrap()
                .rows
                .len() as u64
        };
        assert_eq!(
            report.retained(),
            live,
            "round {round}: garbage left behind"
        );
        assert!(live <= KEYS + 8);
    }
    let metrics = storage.metrics().await.unwrap();
    assert!(
        written > 20 * (KEYS + 8),
        "workload too small ({written} versions)"
    );
    assert!(metrics.gc.passes >= 15);
    assert!(metrics.gc.versions_removed > written / 2);
    assert_eq!(metrics.gc.failed_passes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert continuous GC never removes a version a live snapshot can see.
///
/// Transfers keep the sum of all accounts constant while slow readers scan
/// every account in tiny batches, yielding between them so GC passes run
/// mid-scan; every reader must see the original total.
async fn gc_preserves_snapshots_under_concurrent_transfers() {
    const ACCOUNTS: u64 = 20;
    const TOTAL: i64 = 20 * 100;
    let dir = tempfile::tempdir().unwrap();
    let storage = FjallStorage::open(
        dir.path(),
        StorageOptions {
            gc_interval: Some(Duration::from_millis(1)),
            gc_chunk_versions: 3,
            ..options()
        },
    )
    .unwrap();
    write_all(&storage, 0..ACCOUNTS, 100).await;

    let transfers: Vec<_> = (0..6u64)
        .map(|worker| {
            let storage = storage.clone();
            tokio::spawn(async move {
                let mut rng = worker * 2_654_435_761 + 1;
                for _ in 0..150 {
                    rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    let (a, b) = ((rng >> 33) % ACCOUNTS, (rng >> 17) % ACCOUNTS);
                    if a == b {
                        continue;
                    }
                    let txn = storage
                        .begin(TxnOptions::new(Isolation::Serializable))
                        .await
                        .unwrap();
                    let values = txn.get_many(vec![key(a), key(b)]).await.unwrap();
                    let (va, vb) = (
                        int(values[0].as_ref().unwrap()),
                        int(values[1].as_ref().unwrap()),
                    );
                    txn.put(key(a), value(va - 7)).unwrap();
                    txn.put(key(b), value(vb + 7)).unwrap();
                    let _ = txn.commit().await;
                }
            })
        })
        .collect();
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let storage = storage.clone();
            tokio::spawn(async move {
                let mut checked = 0;
                for _ in 0..25 {
                    let txn = storage
                        .begin(TxnOptions::default().read_only())
                        .await
                        .unwrap();
                    let mut request = ScanRequest::new(KeyRange::all());
                    request.limit_rows = 2;
                    let mut sum = 0;
                    let mut seen = 0;
                    loop {
                        let batch = txn.scan(request.clone()).await.unwrap();
                        seen += batch.rows.len();
                        sum += batch.rows.iter().map(|(_, v)| int(v)).sum::<i64>();
                        tokio::time::sleep(Duration::from_micros(200)).await;
                        match request.resume_after(&batch) {
                            Some(next) => request = next,
                            None => break,
                        }
                    }
                    assert_eq!(seen as u64, ACCOUNTS, "reader lost accounts");
                    assert_eq!(sum, TOTAL, "reader saw an inconsistent total");
                    checked += 1;
                }
                checked
            })
        })
        .collect();
    for task in transfers {
        task.await.unwrap();
    }
    for task in readers {
        assert_eq!(task.await.unwrap(), 25);
    }
    let metrics = storage.metrics().await.unwrap();
    assert!(metrics.gc.versions_removed > 0, "GC never removed anything");
}

#[tokio::test]
/// Assert an over-age transaction stops holding back GC and fails its next operation.
async fn expired_transactions_release_gc() {
    let dir = tempfile::tempdir().unwrap();
    let storage = FjallStorage::open(
        dir.path(),
        StorageOptions {
            max_transaction_age: Duration::from_millis(100),
            ..options()
        },
    )
    .unwrap();
    write_all(&storage, 0..5, 1).await;
    let reader = storage
        .begin(TxnOptions::default().read_only())
        .await
        .unwrap();
    let writer = storage.begin(TxnOptions::default()).await.unwrap();
    writer.put(key(9), value(9)).unwrap();
    write_all(&storage, 0..5, 2).await;

    // While the reader is young, GC keeps the versions it can see.
    let report = storage.run_gc().await.unwrap();
    assert_eq!(report.retained(), 10);
    assert_eq!(
        reader.get_many(vec![key(0)]).await.unwrap(),
        vec![Some(value(1))]
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    let report = storage.run_gc().await.unwrap();
    assert_eq!(
        report.retained(),
        5,
        "expired readers no longer pin old versions"
    );
    assert!(matches!(
        reader.get_many(vec![key(0)]).await,
        Err(Error::TransactionTooOld { .. })
    ));
    assert!(matches!(
        writer.put(key(8), value(8)),
        Err(Error::TransactionTooOld { .. })
    ));
    assert!(matches!(
        writer.commit().await,
        Err(Error::TransactionTooOld { .. })
    ));
    let metrics = storage.metrics().await.unwrap();
    assert_eq!(metrics.expired_transactions, 2);
    assert_eq!(metrics.active_transactions, 0);
}

#[tokio::test]
/// Assert the write-set limit counts final writes and rejects writes that exceed it.
async fn write_set_limit() {
    let dir = tempfile::tempdir().unwrap();
    let storage = FjallStorage::open(
        dir.path(),
        StorageOptions {
            max_write_set_bytes: 100,
            ..options()
        },
    )
    .unwrap();
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let big = Bytes::from(vec![0u8; 40]);
    txn.put(key(1), big.clone()).unwrap(); // 5 + 40
    txn.put(key(2), big.clone()).unwrap(); // 90 in total
    // Overwriting replaces the old size rather than adding to it.
    txn.put(key(2), big.clone()).unwrap();
    assert!(matches!(
        txn.put(key(3), big.clone()),
        Err(Error::WriteSetTooLarge { limit: 100 })
    ));
    // Shrinking a value frees room.
    txn.delete(key(2)).unwrap();
    txn.put(key(3), big).unwrap();
    txn.commit().await.unwrap();
}

#[tokio::test]
/// Assert tombstoned keys disappear entirely and the watermark is persisted.
async fn deleted_keys_vanish_and_watermark_persists() {
    let dir = tempfile::tempdir().unwrap();
    let watermark = {
        let storage = FjallStorage::open(dir.path(), options()).unwrap();
        write_all(&storage, 0..10, 1).await;
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        for i in 0..10 {
            txn.delete(key(i)).unwrap();
        }
        txn.commit().await.unwrap();
        let report = storage.run_gc().await.unwrap();
        assert_eq!(report.examined, 20);
        assert_eq!(report.retained(), 0);
        let metrics = storage.metrics().await.unwrap();
        assert_eq!(metrics.gc.last_pass, report);
        assert!(metrics.disk_space_bytes > 0);
        report.watermark
    };
    let engine = Engine::open(dir.path()).unwrap();
    assert_eq!(engine.gc_watermark().unwrap(), watermark);
}
