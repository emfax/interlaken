//! Model-based tests for the row layer (ADR-0009, milestone M4).
//!
//! Random transactions insert, update and delete rows of a table with a
//! unique and a non-unique index, read them back, scan both indexes, and
//! drop and recreate the non-unique index. Every outcome (success,
//! constraint violation, missing row) and every read is compared against a
//! `BTreeMap` model, and after every transaction the indexes must match the
//! rows exactly.

use std::collections::BTreeMap;

use interlaken_storage::catalog::{self, ColumnDef, IndexSpec, TableDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum, SortOrder};
use interlaken_storage::rows::{self, RowScan};
use interlaken_storage::{
    ConstraintViolation, Durability, Error, FjallStorage, Storage, StorageOptions, Txn, TxnOptions,
};
use proptest::prelude::*;

/// A model row: email (unique, nullable) and team (indexed when the index exists).
type ModelRow = (Option<u8>, u8);

/// One operation.
#[derive(Clone, Debug)]
enum Op {
    Insert(u8, Option<u8>, u8),
    Update(u8, u8, Option<u8>, u8),
    Delete(u8),
    Get(u8),
    ScanEmail(Option<u8>, bool),
    ScanTeam(u8),
    ToggleTeamIndex,
}

/// A transaction: operations, then commit or abort.
#[derive(Clone, Debug)]
struct Script {
    ops: Vec<Op>,
    commit: bool,
}

/// What an operation did.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Done,
    Missing,
    PrimaryKey,
    Unique,
}

/// Construct a row.
fn row(id: u8, (email, team): ModelRow) -> Vec<Datum> {
    vec![
        Datum::Int8(i64::from(id)),
        email.map_or(Datum::Null, |e| Datum::Text(format!("e{e}"))),
        Datum::Int4(i32::from(team)),
    ]
}

/// Generate an operation.
fn op() -> impl Strategy<Value = Op> {
    let email = prop::option::weighted(0.8, 0u8..5);
    prop_oneof![
        4 => (0u8..8, email.clone(), 0u8..3).prop_map(|(i, e, t)| Op::Insert(i, e, t)),
        3 => (0u8..8, 0u8..8, email.clone(), 0u8..3).prop_map(|(o, n, e, t)| Op::Update(o, n, e, t)),
        2 => (0u8..8).prop_map(Op::Delete),
        2 => (0u8..8).prop_map(Op::Get),
        1 => (email, any::<bool>()).prop_map(|(e, r)| Op::ScanEmail(e, r)),
        1 => (0u8..3).prop_map(Op::ScanTeam),
        1 => Just(Op::ToggleTeamIndex),
    ]
}

/// Generate a transaction script.
fn script() -> impl Strategy<Value = Script> {
    (
        prop::collection::vec(op(), 1..12),
        prop::bool::weighted(0.8),
    )
        .prop_map(|(ops, commit)| Script { ops, commit })
}

/// Classify a write result.
fn outcome(result: Result<bool, Error>) -> Outcome {
    match result {
        Ok(true) => Outcome::Done,
        Ok(false) => Outcome::Missing,
        Err(Error::Constraint(ConstraintViolation::PrimaryKey { .. })) => Outcome::PrimaryKey,
        Err(Error::Constraint(ConstraintViolation::Unique { .. })) => Outcome::Unique,
        Err(other) => panic!("unexpected error: {other}"),
    }
}

/// Return `true` if a row other than `except` has `email`.
fn email_taken(model: &BTreeMap<u8, ModelRow>, email: Option<u8>, except: Option<u8>) -> bool {
    email.is_some()
        && model
            .iter()
            .any(|(id, (e, _))| Some(*id) != except && *e == email)
}

/// Return every primary key from an index scan, following resume keys.
async fn scan_index(txn: &Txn, def: &TableDef, name: &str, mut scan: RowScan) -> Vec<Vec<Datum>> {
    let index = def.index(name).unwrap();
    scan.limit_rows = 2;
    let mut out = Vec::new();
    loop {
        let batch = rows::scan_index(txn, def, index, &scan).await.unwrap();
        out.extend(batch.primary_keys);
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => return out,
        }
    }
}

/// Apply one operation to the storage and the model and compare.
async fn apply(txn: &Txn, def: &mut TableDef, model: &mut BTreeMap<u8, ModelRow>, op: &Op) {
    match *op {
        Op::Insert(id, email, team) => {
            let actual = outcome(
                rows::insert(txn, def, row(id, (email, team)))
                    .await
                    .map(|()| true),
            );
            let expected = if model.contains_key(&id) {
                Outcome::PrimaryKey
            } else if email_taken(model, email, None) {
                Outcome::Unique
            } else {
                model.insert(id, (email, team));
                Outcome::Done
            };
            assert_eq!(actual, expected, "{op:?}");
        }
        Op::Update(old, new, email, team) => {
            let actual = outcome(
                rows::update(
                    txn,
                    def,
                    &[Datum::Int8(i64::from(old))],
                    row(new, (email, team)),
                )
                .await,
            );
            let expected = match model.get(&old).copied() {
                None => Outcome::Missing,
                Some(_) if new != old && model.contains_key(&new) => Outcome::PrimaryKey,
                Some((old_email, _))
                    if email != old_email && email_taken(model, email, Some(old)) =>
                {
                    Outcome::Unique
                }
                Some(_) => {
                    model.remove(&old);
                    model.insert(new, (email, team));
                    Outcome::Done
                }
            };
            assert_eq!(actual, expected, "{op:?}");
        }
        Op::Delete(id) => {
            let actual = outcome(rows::delete(txn, def, &[Datum::Int8(i64::from(id))]).await);
            let expected = if model.remove(&id).is_some() {
                Outcome::Done
            } else {
                Outcome::Missing
            };
            assert_eq!(actual, expected, "{op:?}");
        }
        Op::Get(id) => {
            let actual = rows::get(txn, def, &[Datum::Int8(i64::from(id))])
                .await
                .unwrap();
            assert_eq!(actual, model.get(&id).map(|r| row(id, *r)), "{op:?}");
        }
        Op::ScanEmail(email, reverse) => {
            let prefix = email
                .map(|e| vec![Datum::Text(format!("e{e}"))])
                .unwrap_or_default();
            let mut scan = RowScan::prefix(prefix);
            scan.reverse = reverse;
            let actual = scan_index(txn, def, "t_email", scan).await;
            // Index order: email ascending with NULLs last, then primary key.
            let mut expected: Vec<(Option<u8>, u8)> = model
                .iter()
                .filter(|(_, (e, _))| email.is_none() || *e == email)
                .map(|(id, (e, _))| (*e, *id))
                .collect();
            expected.sort_by_key(|(e, id)| (e.is_none(), *e, *id));
            if reverse {
                expected.reverse();
            }
            let expected: Vec<Vec<Datum>> = expected
                .into_iter()
                .map(|(_, id)| vec![Datum::Int8(i64::from(id))])
                .collect();
            assert_eq!(actual, expected, "{op:?}");
        }
        Op::ScanTeam(team) => {
            if def.index("t_team").is_none() {
                return;
            }
            let actual = scan_index(
                txn,
                def,
                "t_team",
                RowScan::prefix(vec![Datum::Int4(i32::from(team))]),
            )
            .await;
            let expected: Vec<Vec<Datum>> = model
                .iter()
                .filter(|(_, (_, t))| *t == team)
                .map(|(id, _)| vec![Datum::Int8(i64::from(*id))])
                .collect();
            assert_eq!(actual, expected, "{op:?}");
        }
        Op::ToggleTeamIndex => {
            *def = if def.index("t_team").is_some() {
                catalog::drop_index(txn, "t_team").await.unwrap()
            } else {
                catalog::create_index(
                    txn,
                    IndexSpec {
                        name: "t_team".into(),
                        table: "t".into(),
                        columns: vec![("team".into(), SortOrder::DESC)],
                        unique: false,
                        nulls_distinct: true,
                    },
                )
                .await
                .unwrap()
            };
        }
    }
    // Make this operation's writes visible to the next one's reads.
    txn.next_statement();
}

/// Run every script against fresh storage and the model.
async fn run(scripts: Vec<Script>) {
    let dir = tempfile::tempdir().unwrap();
    let options = StorageOptions {
        pool_threads: 2,
        durability: Durability::Buffered,
        ..StorageOptions::default()
    };
    let storage = FjallStorage::open(dir.path(), options).unwrap();
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    catalog::create_table(
        &txn,
        TableSpec {
            name: "t".into(),
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
    catalog::create_index(
        &txn,
        IndexSpec {
            name: "t_email".into(),
            table: "t".into(),
            columns: vec![("email".into(), SortOrder::ASC)],
            unique: true,
            nulls_distinct: true,
        },
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();

    let mut model: BTreeMap<u8, ModelRow> = BTreeMap::new();
    for script in scripts {
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        let mut def = catalog::table(&txn, "t").await.unwrap().unwrap();
        let mut working = model.clone();
        for op in &script.ops {
            apply(&txn, &mut def, &mut working, op).await;
        }
        let problems = rows::check_indexes(&txn, &def).await.unwrap();
        assert!(problems.is_empty(), "before commit: {problems:?}");
        if script.commit {
            txn.commit().await.unwrap();
            model = working;
        } else {
            txn.abort();
        }

        let check = storage
            .begin(TxnOptions::default().read_only())
            .await
            .unwrap();
        let def = catalog::table(&check, "t").await.unwrap().unwrap();
        let problems = rows::check_indexes(&check, &def).await.unwrap();
        assert!(problems.is_empty(), "after commit: {problems:?}");
        let all = rows::scan(&check, &def, &RowScan::all())
            .await
            .unwrap()
            .rows;
        let expected: Vec<Vec<Datum>> = model.iter().map(|(id, r)| row(*id, *r)).collect();
        assert_eq!(all, expected);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Row-layer outcomes, reads and index contents match the model.
    #[test]
    fn rows_match_model(scripts in prop::collection::vec(script(), 1..8)) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(scripts));
    }
}
