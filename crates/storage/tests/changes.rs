//! The change log (ADR-0040 phase 2).
//!
//! Random transactions write rows one at a time and in batches, move rows
//! by changing their primary keys, delete rows and insert-then-delete
//! within one transaction, and some roll back. Replaying the change log
//! onto an empty copy must give the table's rows, and every write's prior
//! state must match the copy: inserts and undone inserts of absent keys,
//! updates and deletes of present ones.

use std::collections::BTreeMap;
use std::time::Duration;

use interlaken_storage::catalog::{self, ColumnDef, TableDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum, SortOrder, decode_row, decode_tuple};
use interlaken_storage::rows::{self, RowScan};
use interlaken_storage::{
    ChangeEntry, ChangeRetention, Durability, Error, FjallStorage, Prior, Storage, StorageOptions,
    Timestamp, Txn, TxnOptions,
};

/// Return options keeping `retention`, with no background GC.
fn options(retention: ChangeRetention) -> StorageOptions {
    StorageOptions {
        durability: Durability::Buffered,
        gc_interval: None,
        metrics_interval: None,
        change_retention: retention,
        ..StorageOptions::default()
    }
}

/// Create table `t(id int8, n int8)`.
async fn create(storage: &FjallStorage) -> TableDef {
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let table = catalog::create_table(
        &txn,
        TableSpec {
            name: "t".into(),
            columns: vec![
                ColumnDef::new("id", ColumnType::Int8),
                ColumnDef::new("n", ColumnType::Int8),
            ],
            primary_key: vec!["id".into()],
        },
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    table
}

/// Return every row of `table` by primary key.
async fn contents(storage: &FjallStorage, table: &TableDef) -> BTreeMap<i64, i64> {
    let txn = storage
        .begin(TxnOptions::default().read_only())
        .await
        .unwrap();
    let mut scan = RowScan::all();
    let mut out = BTreeMap::new();
    loop {
        let batch = rows::scan(&txn, table, &scan).await.unwrap();
        for row in batch.rows {
            out.insert(int(&row[0]), int(&row[1]));
        }
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => return out,
        }
    }
}

/// Return an `int8` value.
fn int(datum: &Datum) -> i64 {
    match datum {
        Datum::Int8(v) => *v,
        other => panic!("not an int8: {other:?}"),
    }
}

/// A small deterministic random source.
struct Lcg(u64);

impl Lcg {
    /// Return a value below `n`.
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.0 >> 33) % n
    }
}

/// Run one random transaction on `table`; roll it back sometimes.
async fn random_transaction(storage: &FjallStorage, table: &TableDef, rng: &mut Lcg) {
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    for _ in 0..=rng.below(4) {
        let id = rng.below(12) as i64;
        let n = rng.below(1_000) as i64;
        let row = |id: i64| vec![Datum::Int8(id), Datum::Int8(n)];
        // Failures (duplicate keys, missing rows) leave the statement's
        // writes undone, as the engine does.
        let _ = match rng.below(6) {
            0 => rows::insert(&txn, table, row(id)).await.map(|()| true),
            1 => rows::update(&txn, table, &[Datum::Int8(id)], row(id)).await,
            2 => {
                let to = rng.below(12) as i64;
                rows::update(&txn, table, &[Datum::Int8(id)], row(to)).await
            }
            3 => rows::delete(&txn, table, &[Datum::Int8(id)]).await,
            4 => rows::insert_many(&txn, table, vec![row(id), row(id + 20)])
                .await
                .map(|()| true),
            _ => {
                let fresh = 100 + rng.below(10) as i64;
                let inserted = rows::insert(&txn, table, row(fresh)).await;
                txn.next_statement();
                inserted.and(Ok(rows::delete(&txn, table, &[Datum::Int8(fresh)])
                    .await
                    .unwrap()))
            }
        };
        txn.next_statement();
    }
    if rng.below(5) == 0 {
        txn.abort();
    } else {
        let _ = txn.commit().await;
    }
}

/// Apply `entries` to `copy`, checking each write's prior state against it.
fn replay(
    copy: &mut BTreeMap<i64, i64>,
    table: &TableDef,
    entries: &[std::sync::Arc<ChangeEntry>],
) {
    let types = table.types();
    for entry in entries {
        for write in &entry.writes {
            assert_eq!(write.table_id(), table.id().get());
            let key = decode_tuple(&write.key, &[SortOrder::ASC; 3]).unwrap();
            let id = int(&key[2]);
            let present = copy.contains_key(&id);
            match (&write.value, write.prior) {
                (Some(value), prior) => {
                    let expected = if present {
                        Prior::Existed
                    } else {
                        Prior::Absent
                    };
                    assert_eq!(prior, expected, "put of {id} at {}", entry.ts);
                    copy.insert(id, int(&decode_row(value, &types).unwrap().1[1]));
                }
                (None, Prior::Existed) => {
                    assert!(present, "delete of absent {id} at {}", entry.ts);
                    copy.remove(&id);
                }
                (None, Prior::Absent) => assert!(!present, "undone insert of present {id}"),
                (None, Prior::Unknown) => panic!("row deletes record their prior state"),
            }
        }
    }
}

/// Read every change after `after` in pages of `page`.
async fn read_all(
    storage: &FjallStorage,
    mut after: Timestamp,
    page: usize,
) -> Vec<std::sync::Arc<ChangeEntry>> {
    let mut out = Vec::new();
    loop {
        let entries = storage.read_changes(after, page).await.unwrap();
        let Some(last) = entries.last() else {
            return out;
        };
        after = last.ts;
        out.extend(entries);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert replaying the change log onto an empty copy gives the table, with
/// every prior state right, read whole and in pages.
async fn replay_matches_the_table() {
    let dir = tempfile::tempdir().unwrap();
    let storage = FjallStorage::open(dir.path(), options(ChangeRetention::DEFAULT)).unwrap();
    let table = create(&storage).await;
    let start = storage.visible_ts();
    let mut rng = Lcg(7);
    for _ in 0..300 {
        random_transaction(&storage, &table, &mut rng).await;
    }
    let actual = contents(&storage, &table).await;
    for page in [1, 7, 10_000] {
        let entries = read_all(&storage, start, page).await;
        assert!(
            entries.windows(2).all(|w| w[0].ts < w[1].ts),
            "commit order"
        );
        let mut copy = BTreeMap::new();
        replay(&mut copy, &table, &entries);
        assert_eq!(copy, actual, "pages of {page}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a memory log past its bytes reports a gap below its floor, and
/// one that is off reports itself unavailable.
async fn gaps_and_off() {
    let dir = tempfile::tempdir().unwrap();
    let storage =
        FjallStorage::open(dir.path(), options(ChangeRetention::Memory { bytes: 200 })).unwrap();
    let table = create(&storage).await;
    let start = storage.visible_ts();
    let mut rng = Lcg(3);
    for _ in 0..100 {
        random_transaction(&storage, &table, &mut rng).await;
    }
    assert!(storage.change_floor() > start);
    let error = storage.read_changes(start, 10).await.unwrap_err();
    assert!(matches!(error, Error::ChangeGap { .. }), "{error}");
    assert_eq!(error.sqlstate(), "55000");
    // From the floor, nothing is missing.
    storage
        .read_changes(storage.change_floor(), 10)
        .await
        .unwrap();
    drop(storage);

    let storage = FjallStorage::open(dir.path(), options(ChangeRetention::Off)).unwrap();
    assert!(matches!(
        storage.read_changes(Timestamp::ZERO, 10).await,
        Err(Error::ChangesUnavailable)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a durable log survives reopening, is trimmed by bytes, and is
/// dropped by an open that doesn't keep it.
async fn durable_log() {
    let dir = tempfile::tempdir().unwrap();
    let durable = ChangeRetention::Durable {
        age: Duration::from_secs(3600),
        bytes: 1 << 30,
    };
    let storage = FjallStorage::open(dir.path(), options(durable)).unwrap();
    let table = create(&storage).await;
    let start = storage.visible_ts();
    let mut rng = Lcg(11);
    for _ in 0..100 {
        random_transaction(&storage, &table, &mut rng).await;
    }
    let actual = contents(&storage, &table).await;
    drop(storage);

    // Reopened, memory is empty and the durable log answers.
    let storage = FjallStorage::open(dir.path(), options(durable)).unwrap();
    assert!(
        storage.change_floor() <= start,
        "the log began before the table"
    );
    let mut copy = BTreeMap::new();
    replay(&mut copy, &table, &read_all(&storage, start, 9).await);
    assert_eq!(copy, actual);
    drop(storage);

    // Trimmed to a few hundred bytes by a GC pass.
    let small = ChangeRetention::Durable {
        age: Duration::from_secs(3600),
        bytes: 300,
    };
    let storage = FjallStorage::open(dir.path(), options(small)).unwrap();
    storage.run_gc().await.unwrap();
    let floor = storage.change_floor();
    assert!(floor > start);
    assert!(matches!(
        storage.read_changes(start, 10).await,
        Err(Error::ChangeGap { .. })
    ));
    assert!(!read_all(&storage, floor, 10).await.is_empty());
    drop(storage);

    // An open in memory drops the durable log; a later durable open
    // starts afresh at the last commit.
    drop(FjallStorage::open(dir.path(), options(ChangeRetention::DEFAULT)).unwrap());
    let storage = FjallStorage::open(dir.path(), options(durable)).unwrap();
    assert_eq!(storage.change_floor(), storage.visible_ts());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a waiter for the next commit wakes when it arrives.
async fn waiting_for_the_next_commit() {
    let dir = tempfile::tempdir().unwrap();
    let storage = FjallStorage::open(dir.path(), options(ChangeRetention::DEFAULT)).unwrap();
    let table = create(&storage).await;
    let now = storage.visible_ts();
    let waiter = tokio::spawn(storage.wait_after(now));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished());
    let txn: Txn = storage.begin(TxnOptions::default()).await.unwrap();
    rows::insert(&txn, &table, vec![Datum::Int8(1), Datum::Int8(1)])
        .await
        .unwrap();
    txn.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a history pin keeps the versions after it through GC, so reads
/// at past commits see them, and reads below the GC floor are refused.
async fn history_pins_hold_versions() {
    let dir = tempfile::tempdir().unwrap();
    let storage = FjallStorage::open(dir.path(), options(ChangeRetention::DEFAULT)).unwrap();
    let table = create(&storage).await;
    let write = |n: i64| {
        let storage = &storage;
        let table = &table;
        async move {
            let txn = storage.begin(TxnOptions::default()).await.unwrap();
            if n == 0 {
                rows::insert(&txn, table, vec![Datum::Int8(1), Datum::Int8(n)])
                    .await
                    .unwrap();
            } else {
                rows::update(
                    &txn,
                    table,
                    &[Datum::Int8(1)],
                    vec![Datum::Int8(1), Datum::Int8(n)],
                )
                .await
                .unwrap();
            }
            txn.commit().await.unwrap().commit_ts.unwrap()
        }
    };
    let first = write(0).await;
    let pin = storage.pin_history(first).unwrap();
    let stamps = [write(1).await, write(2).await, write(3).await];
    storage.run_gc().await.unwrap();
    let value_at = |txn: Txn| {
        let table = &table;
        async move {
            let row = rows::get(&txn, table, &[Datum::Int8(1)])
                .await
                .unwrap()
                .unwrap();
            int(&row[1])
        }
    };
    for (n, ts) in stamps.iter().enumerate() {
        let txn = storage.begin_at(*ts, TxnOptions::default()).unwrap();
        assert_eq!(value_at(txn).await, n as i64 + 1);
        let txn = storage.begin_before(*ts, TxnOptions::default()).unwrap();
        assert_eq!(value_at(txn).await, n as i64);
    }
    drop(pin);
    storage.run_gc().await.unwrap();
    assert!(matches!(
        storage.begin_at(stamps[0], TxnOptions::default()),
        Err(Error::ChangeGap { .. })
    ));
    assert!(storage.pin_history(first).is_err());
    storage
        .begin_at(storage.visible_ts(), TxnOptions::default())
        .unwrap();
}
