//! DML, statement atomicity, Halloween protection and index access paths (ADR-0026, ADR-0023).

use std::sync::Arc;

use arrow_array::RecordBatch;
use interlaken_exec::{Access, ExecError, Op, Physical, Transaction, datum_at, plan};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::{Datum, ExecOptions};
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan};
use interlaken_storage::catalog::{self, ColumnDef, IndexSpec, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum as Stored, SortOrder};
use interlaken_storage::rows;
use interlaken_storage::{Durability, FjallStorage, Storage, StorageOptions, TxnOptions};

/// The IR catalog: `t(id int8 primary key, a int4, b text)` with indexes on `a` and unique `b`.
fn ir_catalog() -> Arc<InMemoryCatalog> {
    Arc::new(
        parse_catalog(
            "(catalog (builtins)
               (table t :id 100 (column id int8) (column a (option int4)) (column b (option text)) (primary-key id)
                 (index t_a a)
                 (index t_b b :unique true)))",
        )
        .unwrap(),
    )
}

/// Open storage with `t` and its indexes, empty.
async fn open(dir: &std::path::Path) -> FjallStorage {
    let storage = FjallStorage::open(
        dir,
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
                ColumnDef::new("a", ColumnType::Int4),
                ColumnDef::new("b", ColumnType::Text),
            ],
            primary_key: vec!["id".into()],
        },
    )
    .await
    .unwrap();
    for (name, column, unique) in [("t_a", "a", false), ("t_b", "b", true)] {
        let spec = IndexSpec {
            name: name.into(),
            table: "t".into(),
            columns: vec![(column.into(), SortOrder::ASC)],
            unique,
        };
        catalog::create_index(&txn, spec).await.unwrap();
    }
    txn.commit().await.unwrap();
    storage
}

/// Plan `text` against the catalog.
fn physical(text: &str, catalog: &InMemoryCatalog) -> Physical {
    let resolved = resolve(&parse_plan(text).unwrap(), catalog)
        .unwrap_or_else(|e| panic!("{e}: {text}"))
        .plan;
    plan(&resolved, catalog).unwrap()
}

/// Return the rows of batches.
fn rows(batches: &[RecordBatch]) -> Vec<Vec<Datum>> {
    batches
        .iter()
        .flat_map(|b| {
            (0..b.num_rows()).map(move |r| {
                b.columns()
                    .iter()
                    .map(|c| datum_at(c.as_ref(), r).unwrap())
                    .collect()
            })
        })
        .collect()
}

/// Begin a statement-running transaction.
async fn begin(storage: &FjallStorage, catalog: &Arc<InMemoryCatalog>) -> Transaction {
    let shared: Arc<dyn Catalog> = catalog.clone();
    Transaction::new(
        storage.begin(TxnOptions::default()).await.unwrap(),
        shared,
        ExecOptions::default(),
    )
}

/// Run `text` and return its rows.
async fn run(
    txn: &mut Transaction,
    text: &str,
    catalog: &InMemoryCatalog,
) -> Result<Vec<Vec<Datum>>, ExecError> {
    txn.execute(&physical(text, catalog), Vec::new())
        .await
        .map(|b| rows(&b))
}

/// Return every row of `t`, ordered by id.
async fn table(txn: &mut Transaction, catalog: &InMemoryCatalog) -> Vec<Vec<Datum>> {
    run(txn, "(plan (sort (get t) (asc id)))", catalog)
        .await
        .unwrap()
}

/// Return the access path of the scan under a plan's filter.
fn access(p: &Physical) -> &Access {
    match &p.op {
        Op::TableScan { access, .. } => access,
        Op::Filter { input, .. } | Op::Project { input, .. } => access(input),
        other => panic!("no scan under {other:?}"),
    }
}

#[tokio::test]
/// Assert a failing statement leaves no writes, aborts the transaction, and a savepoint recovers it.
async fn statement_atomicity_and_aborted_state() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = ir_catalog();
    let storage = open(dir.path()).await;
    let mut txn = begin(&storage, &catalog).await;

    let inserted = run(&mut txn, "(plan (insert t (values (columns x y z) (row 1 10 \"one\") (row 2 20 \"two\")) (columns id a b)))", &catalog).await.unwrap();
    assert_eq!(inserted.len(), 2, "one (empty) result row per inserted row");
    let savepoint = txn.savepoint().unwrap();

    // The third row repeats id 1: the whole statement fails and none of its rows remain.
    let failed = run(
        &mut txn,
        "(plan (insert t (values (columns x y z) (row 3 30 \"three\") (row 4 40 \"four\") (row 1 50 \"dup\")) (columns id a b)))",
        &catalog,
    )
    .await
    .unwrap_err();
    assert_eq!(failed.sqlstate(), "23505", "{failed}");
    assert!(txn.is_aborted());
    // The statement's own rollback already removed its rows 3 and 4.
    let stored = catalog::table(txn.txn(), "t").await.unwrap().unwrap();
    txn.txn().next_statement();
    for id in [3, 4] {
        assert!(
            rows::get(txn.txn(), &stored, &[Stored::Int8(id)])
                .await
                .unwrap()
                .is_none(),
            "row {id} survived"
        );
    }
    let next = run(&mut txn, "(plan (get t))", &catalog).await.unwrap_err();
    assert!(matches!(next, ExecError::TransactionAborted), "{next}");
    assert_eq!(next.sqlstate(), "25P02");
    assert!(txn.savepoint().is_err());

    txn.rollback_to(savepoint).unwrap();
    assert!(!txn.is_aborted());
    let ids: Vec<Datum> = table(&mut txn, &catalog)
        .await
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(ids, [Datum::Int8(1), Datum::Int8(2)]);

    // A runtime error mid-update also undoes the statement's earlier writes.
    let err = run(
        &mut txn,
        "(plan (update t (filter (get t) true) (set a (/ (int4 100) (- a (int4 20))))))",
        &catalog,
    )
    .await
    .unwrap_err();
    assert_eq!(err.sqlstate(), "22012");
    // A commit of an aborted transaction rolls back.
    assert!(matches!(
        txn.commit().await,
        Err(ExecError::TransactionAborted)
    ));
    let mut txn = begin(&storage, &catalog).await;
    assert!(
        table(&mut txn, &catalog).await.is_empty(),
        "nothing was committed"
    );
    txn.rollback();
}

#[tokio::test]
/// Assert updates that move rows forward in the scanned order touch each row exactly once.
async fn halloween_protection() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = ir_catalog();
    let storage = open(dir.path()).await;
    let mut txn = begin(&storage, &catalog).await;
    let n = 2500;
    let values: String = (1..=n)
        .map(|i| format!(" (row {i} {} \"r{i}\")", i % 50))
        .collect();
    run(
        &mut txn,
        &format!("(plan (insert t (values (columns x y z){values}) (columns id a b)))"),
        &catalog,
    )
    .await
    .unwrap();

    // Moving every primary key past the end of the table: each row moves once.
    let moved = run(
        &mut txn,
        "(plan (update t (get t) (set id (+ id 100000)) (returning id)))",
        &catalog,
    )
    .await
    .unwrap();
    assert_eq!(moved.len(), n);
    let ids: Vec<Datum> = table(&mut txn, &catalog)
        .await
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(
        ids,
        (1..=n)
            .map(|i| Datum::Int8(i as i64 + 100_000))
            .collect::<Vec<_>>()
    );

    // An update driven by an index on `a` that moves rows forward in that index.
    let update = physical(
        "(plan (update t (filter (get t) (>= a (int4 0))) (set a (+ a (int4 50)))))",
        &catalog,
    );
    let Op::Update { dml, .. } = &update.op else {
        panic!("expected an update")
    };
    assert!(
        matches!(access(&dml.input), Access::Index { name, .. } if name.as_str() == "t_a"),
        "expected the index on a"
    );
    let updated = txn.execute(&update, Vec::new()).await.unwrap();
    assert_eq!(rows(&updated).len(), n);
    let after = table(&mut txn, &catalog).await;
    assert!(after.iter().all(|r| matches!((&r[0], &r[1]), (Datum::Int8(id), Datum::Int4(a)) if i64::from(*a) == (id - 100_000) % 50 + 50)));
    txn.commit().await.unwrap();
}

#[tokio::test]
/// Assert RETURNING, deletes, unique indexes and every access path return the right rows.
async fn dml_returning_and_access_paths() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = ir_catalog();
    let storage = open(dir.path()).await;
    let mut txn = begin(&storage, &catalog).await;
    let values: String = (1..=40)
        .map(|i| format!(" (row {i} {} \"r{i}\")", i % 5))
        .collect();
    run(
        &mut txn,
        &format!("(plan (insert t (values (columns x y z){values}) (columns id a b)))"),
        &catalog,
    )
    .await
    .unwrap();

    let returned = run(
        &mut txn,
        "(plan (delete t (filter (get t) (= a (int4 3))) (returning b (* id 2))))",
        &catalog,
    )
    .await
    .unwrap();
    assert_eq!(returned.len(), 8);
    assert!(returned.contains(&vec![Datum::Text("r3".into()), Datum::Int8(6)]));

    // A unique index rejects a duplicate value, and the statement is undone.
    let savepoint = txn.savepoint().unwrap();
    let err = run(
        &mut txn,
        "(plan (update t (filter (get t) (= id 1)) (set b \"r2\")))",
        &catalog,
    )
    .await
    .unwrap_err();
    assert_eq!(err.sqlstate(), "23505");
    txn.rollback_to(savepoint).unwrap();

    // Each filter picks the expected path and returns what a full scan would.
    let cases: [(&str, &str); 7] = [
        ("(= id 7)", "primary"),
        ("(and (>= id 10) (< id 13))", "primary"),
        ("(= b \"r9\")", "index t_b"),
        ("(= a (int4 4))", "index t_a"),
        ("(and (> a (int4 1)) (<= a (int4 2)))", "index t_a"),
        // Ranges on both keys: the primary key wins the tie, needing no second lookup.
        ("(and (> a (int4 1)) (<= a (int4 2)) (> id 30))", "primary"),
        ("(< (+ id 1) 3)", "full"),
    ];
    for (predicate, expected) in cases {
        let p = physical(&format!("(plan (filter (get t) {predicate}))"), &catalog);
        let path = match access(&p) {
            Access::Full => "full".to_owned(),
            Access::Primary(_) => "primary".to_owned(),
            Access::Index { name, .. } => format!("index {name}"),
        };
        assert_eq!(path, expected, "{predicate}");
        let mut got = rows(&txn.execute(&p, Vec::new()).await.unwrap());
        got.sort_by_key(|r| match r[0] {
            Datum::Int8(v) => v,
            _ => 0,
        });
        let all = table(&mut txn, &catalog).await;
        let want: Vec<Vec<Datum>> = run(&mut txn, &format!("(plan (sort (filter (project (get t) (as id id) (as a a) (as b b)) {predicate}) (asc id)))"), &catalog).await.unwrap();
        assert_eq!(got, want, "{predicate}");
        assert!(want.len() <= all.len());
    }

    // A NULL key value matches nothing, and a parameter can supply a key.
    let p = physical("(plan (filter (get t) (= a (cast null int4))))", &catalog);
    assert!(rows(&txn.execute(&p, Vec::new()).await.unwrap()).is_empty());
    let p = physical(
        "(plan (filter (get t) (= id (param 1 (option int8)))))",
        &catalog,
    );
    assert!(matches!(access(&p), Access::Primary(_)));
    let got = rows(&txn.execute(&p, vec![Datum::Int8(12)]).await.unwrap());
    assert_eq!(
        got,
        [vec![
            Datum::Int8(12),
            Datum::Int4(2),
            Datum::Text("r12".into())
        ]]
    );
    txn.commit().await.unwrap();
}
