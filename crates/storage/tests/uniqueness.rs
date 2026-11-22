//! Concurrent uniqueness and index consistency (ADR-0009 phase 4, milestone M4).
//!
//! Many tasks on a multi-threaded runtime insert, update and delete rows
//! whose unique values collide, at every isolation level, while a DDL task
//! may build an index at the same time. Afterwards the table must hold no
//! duplicate unique values, every committed insert must be present, and
//! every index must match the rows exactly.

use std::collections::BTreeSet;

use interlaken_storage::catalog::{self, ColumnDef, IndexSpec, TableDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum, SortOrder};
use interlaken_storage::rows::{self, RowScan};
use interlaken_storage::{
    ConstraintViolation, Durability, Error, FjallStorage, Isolation, Storage, StorageOptions,
    TxnOptions,
};

/// Concurrent workers.
const WORKERS: u64 = 12;

/// Distinct email values; fewer than the attempts, so inserts collide.
const EMAILS: u64 = 40;

/// Open storage in `dir` and create `accounts` with a unique index on email.
async fn setup(dir: &tempfile::TempDir) -> FjallStorage {
    let options = StorageOptions {
        durability: Durability::Buffered,
        ..StorageOptions::default()
    };
    let storage = FjallStorage::open(dir.path(), options).unwrap();
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    catalog::create_table(
        &txn,
        TableSpec {
            name: "accounts".into(),
            columns: vec![
                ColumnDef::new("id", ColumnType::Int8),
                ColumnDef::new("email", ColumnType::Text),
                ColumnDef::new("team", ColumnType::Int4),
            ],
            primary_key: vec!["id".into()],
        },
    )
    .await
    .unwrap();
    catalog::create_index(&txn, index("accounts_email", "email", true))
        .await
        .unwrap();
    txn.commit().await.unwrap();
    storage
}

/// Construct a one-column index spec on `accounts`.
fn index(name: &str, column: &str, unique: bool) -> IndexSpec {
    IndexSpec {
        name: name.into(),
        table: "accounts".into(),
        columns: vec![(column.into(), SortOrder::ASC)],
        unique,
        nulls_distinct: true,
    }
}

/// Return the next pseudo-random number (xorshift64*), advancing `state`.
fn next(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Construct an `accounts` row.
fn account(id: i64, email: u64, team: i32) -> Vec<Datum> {
    vec![
        Datum::Int8(id),
        Datum::Text(format!("user{email}@example.com")),
        Datum::Int4(team),
    ]
}

/// Read every row and check uniqueness and index consistency; return the rows.
async fn verify(storage: &FjallStorage) -> Vec<Vec<Datum>> {
    let txn = storage
        .begin(TxnOptions::default().read_only())
        .await
        .unwrap();
    let def = catalog::table(&txn, "accounts").await.unwrap().unwrap();
    let problems = rows::check_indexes(&txn, &def).await.unwrap();
    assert!(problems.is_empty(), "index problems: {problems:?}");
    let mut scan = RowScan::all();
    let mut all = Vec::new();
    loop {
        let batch = rows::scan(&txn, &def, &scan).await.unwrap();
        all.extend(batch.rows);
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => break,
        }
    }
    let emails: BTreeSet<String> = all.iter().map(|r| format!("{:?}", r[1])).collect();
    assert_eq!(emails.len(), all.len(), "duplicate email committed");
    all
}

/// Run one worker's mixed inserts, updates and deletes; return the IDs it inserted and kept.
async fn mixed_worker(storage: FjallStorage, id: u64, isolation: Isolation) -> (u64, u64) {
    let mut rng = 0x9E37_79B9_7F4A_7C15 ^ (id + 1).wrapping_mul(0xD1B5_4A32_D192_ED03);
    let mut committed = 0;
    let mut violations = 0;
    for seq in 0..60u64 {
        let txn = storage.begin(TxnOptions::new(isolation)).await.unwrap();
        let def: TableDef = catalog::table(&txn, "accounts").await.unwrap().unwrap();
        let email = next(&mut rng) % EMAILS;
        // An ID some worker inserts (rows are `worker * 1000 + seq`).
        let target = ((next(&mut rng) % WORKERS) * 1000 + next(&mut rng) % 60) as i64;
        let result = match next(&mut rng) % 4 {
            0 | 1 => {
                let row_id = (id * 1000 + seq) as i64;
                rows::insert(&txn, &def, account(row_id, email, (seq % 3) as i32)).await
            }
            2 => rows::update(
                &txn,
                &def,
                &[Datum::Int8(target)],
                account(target, email, (seq % 5) as i32),
            )
            .await
            .map(|_| ()),
            _ => rows::delete(&txn, &def, &[Datum::Int8(target)])
                .await
                .map(|_| ()),
        };
        match result {
            Ok(()) => {}
            Err(Error::Constraint(ConstraintViolation::Unique { .. })) => {
                violations += 1;
                continue;
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
        match txn.commit().await {
            Ok(_) => committed += 1,
            Err(e) if e.is_retryable() => {}
            Err(other) => panic!("unexpected commit error: {other}"),
        }
    }
    (committed, violations)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert concurrent inserts of colliding values commit at most one row per value, at every level.
async fn concurrent_inserts_keep_values_unique() {
    for isolation in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let storage = setup(&dir).await;
        let tasks: Vec<_> = (0..WORKERS)
            .map(|worker| {
                let storage = storage.clone();
                tokio::spawn(async move {
                    let mut rng = worker.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
                    let mut inserted = Vec::new();
                    for seq in 0..30u64 {
                        let email = next(&mut rng) % EMAILS;
                        let row_id = (worker * 1000 + seq) as i64;
                        let txn = storage.begin(TxnOptions::new(isolation)).await.unwrap();
                        let def = catalog::table(&txn, "accounts").await.unwrap().unwrap();
                        match rows::insert(&txn, &def, account(row_id, email, 0)).await {
                            Ok(()) => {}
                            Err(Error::Constraint(ConstraintViolation::Unique { .. })) => continue,
                            Err(other) => panic!("unexpected error: {other}"),
                        }
                        match txn.commit().await {
                            Ok(_) => inserted.push((row_id, email)),
                            Err(e) if e.is_retryable() => {}
                            Err(other) => panic!("unexpected commit error: {other}"),
                        }
                    }
                    inserted
                })
            })
            .collect();
        let mut inserted = Vec::new();
        for task in tasks {
            inserted.extend(task.await.unwrap());
        }
        let all = verify(&storage).await;
        assert_eq!(
            all.len(),
            inserted.len(),
            "{isolation:?}: committed inserts missing"
        );
        for (row_id, email) in inserted {
            assert!(
                all.contains(&account(row_id, email, 0)),
                "{isolation:?}: row {row_id} missing"
            );
        }
        assert!(all.len() as u64 <= EMAILS);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert mixed concurrent inserts, updates and deletes keep values unique and indexes exact.
async fn concurrent_mixed_writes_keep_indexes_exact() {
    for isolation in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let storage = setup(&dir).await;
        let tasks: Vec<_> = (0..WORKERS)
            .map(|worker| tokio::spawn(mixed_worker(storage.clone(), worker, isolation)))
            .collect();
        let mut committed = 0;
        for task in tasks {
            committed += task.await.unwrap().0;
        }
        assert!(committed > 0);
        verify(&storage).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert CREATE INDEX racing with concurrent writes never leaves the index out of step.
async fn create_index_during_writes() {
    for isolation in [Isolation::RepeatableRead, Isolation::Serializable] {
        let dir = tempfile::tempdir().unwrap();
        let storage = setup(&dir).await;
        let writers: Vec<_> = (0..WORKERS)
            .map(|worker| tokio::spawn(mixed_worker(storage.clone(), worker, isolation)))
            .collect();
        let ddl = {
            let storage = storage.clone();
            tokio::spawn(async move {
                let mut attempts = 0;
                loop {
                    attempts += 1;
                    let txn = storage.begin(TxnOptions::new(isolation)).await.unwrap();
                    catalog::create_index(&txn, index("accounts_team", "team", false))
                        .await
                        .unwrap();
                    match txn.commit().await {
                        Ok(_) => return attempts,
                        Err(e) if e.is_retryable() => tokio::task::yield_now().await,
                        Err(other) => panic!("unexpected commit error: {other}"),
                    }
                }
            })
        };
        for writer in writers {
            writer.await.unwrap();
        }
        let attempts = ddl.await.unwrap();
        assert!(attempts >= 1);
        verify(&storage).await;
        let txn = storage
            .begin(TxnOptions::default().read_only())
            .await
            .unwrap();
        let def = catalog::table(&txn, "accounts").await.unwrap().unwrap();
        assert_eq!(def.indexes().len(), 2);
    }
}
