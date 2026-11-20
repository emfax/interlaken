//! Batched row writes against single-row writes (milestone F3, backlog `batched-row-writes`).
//!
//! `insert_many`, `update_many` and `delete_many` must behave exactly like
//! the single-row functions called once per row, in order: the same return
//! values, the same error for the first failing row, and the same rows and
//! index entries afterwards. Random batches collide with existing rows and
//! with each other (duplicate primary keys and unique values within one
//! batch, updates that move a row onto a key an earlier update freed or
//! took, deletes of rows the batch already deleted), over a table with a
//! unique index, a non-unique index and a NOT NULL column.

use interlaken_storage::catalog::{self, ColumnDef, IndexSpec, TableDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum, SortOrder};
use interlaken_storage::rows::{self, RowScan};
use interlaken_storage::{Durability, FjallStorage, Storage, StorageOptions, Txn, TxnOptions};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

/// One batch of writes.
#[derive(Clone, Debug)]
enum Batch {
    /// Rows to insert.
    Insert(Vec<Vec<Datum>>),
    /// (old primary key, new row) pairs.
    Update(Vec<(i64, Vec<Datum>)>),
    /// Primary keys to delete.
    Delete(Vec<i64>),
}

/// Construct a row of `t`: id, a unique `email`, a non-unique `team` and a NOT NULL `score`.
fn row(id: i64, email: Option<u8>, team: Option<u8>, score: Option<i32>) -> Vec<Datum> {
    vec![
        Datum::Int8(id),
        email.map_or(Datum::Null, |e| Datum::Text(format!("e{e}"))),
        team.map_or(Datum::Null, |t| Datum::Int4(i32::from(t))),
        score.map_or(Datum::Null, Datum::Int4),
    ]
}

/// Generate a row with small, colliding values (sometimes violating NOT NULL).
fn any_row() -> impl Strategy<Value = Vec<Datum>> {
    (
        0i64..10,
        proptest::option::weighted(0.8, 0u8..12),
        proptest::option::of(0u8..3),
        proptest::option::weighted(0.95, 0i32..3),
    )
        .prop_map(|(id, email, team, score)| row(id, email, team, score))
}

/// Generate a batch of any kind.
fn any_batch() -> impl Strategy<Value = Batch> {
    prop_oneof![
        proptest::collection::vec(any_row(), 0..6).prop_map(Batch::Insert),
        proptest::collection::vec((0i64..10, any_row()), 0..6).prop_map(Batch::Update),
        proptest::collection::vec(0i64..10, 0..6).prop_map(Batch::Delete),
    ]
}

/// Return every row of `table` in key order.
async fn contents(txn: &Txn, table: &TableDef) -> Vec<Vec<Datum>> {
    let mut scan = RowScan::all();
    let mut out = Vec::new();
    loop {
        let batch = rows::scan(txn, table, &scan).await.unwrap();
        out.extend(batch.rows);
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => return out,
        }
    }
}

/// Apply `batch` one row at a time; return each row's result, stopping at the first error.
async fn singly(txn: &Txn, table: &TableDef, batch: &Batch) -> Result<Vec<bool>, String> {
    let mut out = Vec::new();
    match batch {
        Batch::Insert(rows) => {
            for r in rows {
                rows::insert(txn, table, r.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                out.push(true);
            }
        }
        Batch::Update(updates) => {
            for (pk, r) in updates {
                out.push(
                    rows::update(txn, table, &[Datum::Int8(*pk)], r.clone())
                        .await
                        .map_err(|e| e.to_string())?,
                );
            }
        }
        Batch::Delete(pks) => {
            for pk in pks {
                out.push(
                    rows::delete(txn, table, &[Datum::Int8(*pk)])
                        .await
                        .map_err(|e| e.to_string())?,
                );
            }
        }
    }
    Ok(out)
}

/// Apply `batch` with one batched call.
async fn batched(txn: &Txn, table: &TableDef, batch: &Batch) -> Result<Vec<bool>, String> {
    match batch {
        Batch::Insert(rows) => rows::insert_many(txn, table, rows.clone())
            .await
            .map(|()| vec![true; rows.len()]),
        Batch::Update(updates) => {
            let pairs = updates
                .iter()
                .map(|(pk, r)| (vec![Datum::Int8(*pk)], r.clone()))
                .collect();
            rows::update_many(txn, table, pairs).await
        }
        Batch::Delete(pks) => {
            let keys: Vec<Vec<Datum>> = pks.iter().map(|pk| vec![Datum::Int8(*pk)]).collect();
            rows::delete_many(txn, table, &keys).await
        }
    }
    .map_err(|e| e.to_string())
}

#[test]
/// Assert batched writes return, fail and leave rows and indexes exactly as single-row writes do.
fn batched_writes_match_single_row_writes() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let storage = runtime.block_on(async {
        let storage = FjallStorage::open(
            dir.path(),
            StorageOptions {
                durability: Durability::Buffered,
                gc_interval: None,
                ..StorageOptions::default()
            },
        )
        .unwrap();
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        catalog::create_table(
            &txn,
            TableSpec {
                name: "t".into(),
                columns: vec![
                    ColumnDef::new("id", ColumnType::Int8),
                    ColumnDef::new("email", ColumnType::Text),
                    ColumnDef::new("team", ColumnType::Int4),
                    ColumnDef::new("score", ColumnType::Int4).not_null(),
                ],
                primary_key: vec!["id".into()],
            },
        )
        .await
        .unwrap();
        for (name, column, unique) in [("t_email", "email", true), ("t_team", "team", false)] {
            catalog::create_index(
                &txn,
                IndexSpec {
                    name: name.into(),
                    table: "t".into(),
                    columns: vec![(column.into(), SortOrder::ASC)],
                    unique,
                },
            )
            .await
            .unwrap();
        }
        txn.commit().await.unwrap();
        storage
    });
    let cases = 600;
    let (failed, changed) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(
        &(
            proptest::collection::vec(any_row(), 0..6),
            proptest::collection::vec(any_batch(), 1..4),
        ),
        |(initial, batches)| {
            let (single, batch) = runtime.block_on(async {
                let mut outcomes = Vec::new();
                for run_batched in [false, true] {
                    let txn = storage.begin(TxnOptions::default()).await.unwrap();
                    let table = catalog::table(&txn, "t").await.unwrap().unwrap();
                    for r in &initial {
                        // Initial rows that collide are skipped, identically in both runs.
                        let _ = rows::insert(&txn, &table, r.clone()).await;
                    }
                    txn.next_statement();
                    let mut results = Vec::new();
                    for b in &batches {
                        let result = if run_batched {
                            batched(&txn, &table, b).await
                        } else {
                            singly(&txn, &table, b).await
                        };
                        let stop = result.is_err();
                        results.push(result);
                        // A failed statement is rolled back by its caller; stop here.
                        if stop {
                            break;
                        }
                        txn.next_statement();
                    }
                    txn.next_statement();
                    let rows_after = contents(&txn, &table).await;
                    let index_problems = rows::check_indexes(&txn, &table).await.unwrap();
                    txn.abort();
                    outcomes.push((results, rows_after, index_problems));
                }
                let batch = outcomes.pop().unwrap();
                (outcomes.pop().unwrap(), batch)
            });
            prop_assert!(single.2.is_empty(), "indexes inconsistent: {:?}", single.2);
            prop_assert!(
                batch.2.is_empty(),
                "batched indexes inconsistent: {:?}",
                batch.2
            );
            prop_assert_eq!(&single.0, &batch.0, "results differ for {:?}", batches);
            prop_assert_eq!(&single.1, &batch.1, "rows differ for {:?}", batches);
            if single.0.iter().any(Result::is_err) {
                failed.set(failed.get() + 1);
            }
            if single.0.iter().flatten().flatten().any(|b| *b) {
                changed.set(changed.get() + 1);
            }
            Ok(())
        },
    )
    .unwrap();
    let (failed, changed) = (failed.get(), changed.get());
    eprintln!("{failed} of {cases} cases failed a batch, {changed} changed rows");
    // These guard the generator, not the code under test: they fail only if
    // it stops producing failing or row-changing batches. Over 60 runs of
    // 600 random cases, failing cases averaged 262 (sd 12, min 226) and
    // row-changing cases 215 (sd 12, min 187), so both thresholds sit at least
    // nine standard deviations below their means and can't fail by chance.
    assert!(failed * 5 >= cases, "too few failing batches: {failed}");
    assert!(
        changed * 6 >= cases,
        "too few batches changed rows: {changed}"
    );
}
