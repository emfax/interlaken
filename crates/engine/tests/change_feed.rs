//! Change feeds (ADR-0040 phase 3).

mod support;

use std::time::Duration;

use interlaken_engine::{
    Change, ChangeBatch, ChangeFeed, ChangeFilter, ChangeKind, CommitTs, Engine, EngineOptions,
};
use interlaken_ir::interp::Datum;
use interlaken_storage::{ChangeRetention, StorageOptions, TxnOptions};
use support::session::{admin, create, options, session, table, ty};

/// Create a database with tables `orders(id, qty)` and `notes(id, body)`.
async fn database(dir: &std::path::Path) -> Engine {
    let engine = create(dir).await;
    let mut s = session(&engine, admin());
    s.create_table(&table(
        "orders",
        &[("id", "int8"), ("qty", "int8")],
        &["id"],
    ))
    .await
    .unwrap();
    s.create_table(&table(
        "notes",
        &[("id", "int8"), ("body", "text")],
        &["id"],
    ))
    .await
    .unwrap();
    engine
}

/// Run `statement` as the administrator and return its commit timestamp.
async fn run(engine: &Engine, statement: &str) -> Option<CommitTs> {
    session(engine, admin())
        .query(statement, Vec::new())
        .await
        .unwrap()
        .commit_ts
}

/// Return the next batch, failing the test if none comes within 5 seconds.
async fn next(feed: &mut ChangeFeed) -> ChangeBatch {
    tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .expect("a batch within 5 seconds")
        .expect("the feed continues")
        .unwrap()
}

/// Return a change's kind and key, for comparing.
fn kind_key(change: &Change) -> (ChangeKind, Vec<Datum>) {
    (change.kind, change.key.clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a feed delivers each transaction's changes to its tables in
/// commit order: inserts, updates, deletes and primary-key moves, with rows,
/// skipping transactions that touch only other tables.
async fn delivers_committed_changes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut feed = engine
        .changes(ChangeFilter::tables(["orders"]), None)
        .await
        .unwrap();

    let first = run(
        &engine,
        "(plan (insert orders (values (columns a b) (row 1 10) (row 2 20)) (columns id qty)))",
    )
    .await
    .unwrap();
    run(
        &engine,
        "(plan (insert notes (values (columns a b) (row 1 \"x\")) (columns id body)))",
    )
    .await;
    let second = run(
        &engine,
        "(plan (update orders (filter (get orders :as o) (= o.id 1)) (set qty (+ o.qty 1))))",
    )
    .await
    .unwrap();
    let third = run(
        &engine,
        "(plan (delete orders (filter (get orders :as o) (= o.id 2))))",
    )
    .await
    .unwrap();
    let fourth = run(
        &engine,
        "(plan (update orders (filter (get orders :as o) (= o.id 1)) (set id 5)))",
    )
    .await
    .unwrap();

    let batch = next(&mut feed).await;
    assert_eq!(batch.commit_ts, first);
    assert_eq!(
        batch.changes.iter().map(kind_key).collect::<Vec<_>>(),
        [
            (ChangeKind::Insert, vec![Datum::Int8(1)]),
            (ChangeKind::Insert, vec![Datum::Int8(2)]),
        ]
    );
    assert_eq!(batch.changes[0].table, "orders");
    assert_eq!(
        batch.changes[1].row,
        Some(vec![Datum::Int8(2), Datum::Int8(20)])
    );

    let batch = next(&mut feed).await;
    assert_eq!(batch.commit_ts, second, "the notes insert is skipped");
    assert_eq!(batch.changes[0].kind, ChangeKind::Update);
    assert_eq!(
        batch.changes[0].row,
        Some(vec![Datum::Int8(1), Datum::Int8(11)])
    );

    let batch = next(&mut feed).await;
    assert_eq!(batch.commit_ts, third);
    assert_eq!(
        (batch.changes[0].kind, batch.changes[0].row.clone()),
        (ChangeKind::Delete, None)
    );

    let batch = next(&mut feed).await;
    assert_eq!(batch.commit_ts, fourth);
    assert_eq!(
        batch.changes.iter().map(kind_key).collect::<Vec<_>>(),
        [
            (ChangeKind::Delete, vec![Datum::Int8(1)]),
            (ChangeKind::Insert, vec![Datum::Int8(5)]),
        ]
    );
    assert_eq!(feed.position(), fourth);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a feed resumed from a saved position delivers what follows it,
/// that `all` follows every user table, and `keys_only` leaves rows out.
async fn resumes_and_filters() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let start = engine.visible_ts();
    let mut stamps = Vec::new();
    for id in 0..5 {
        stamps.push(
            run(
                &engine,
                &format!(
                    "(plan (insert orders (values (columns a b) (row {id} 0)) (columns id qty)))"
                ),
            )
            .await
            .unwrap(),
        );
    }
    run(
        &engine,
        "(plan (insert notes (values (columns a b) (row 1 \"x\")) (columns id body)))",
    )
    .await;

    let mut resumed = engine
        .changes(
            ChangeFilter::tables(["orders"]).keys_only(),
            Some(stamps[2]),
        )
        .await
        .unwrap();
    for expected in &stamps[3..] {
        let batch = next(&mut resumed).await;
        assert_eq!(batch.commit_ts, *expected);
        assert_eq!(batch.changes[0].row, None);
    }

    let mut all = engine
        .changes(ChangeFilter::all(), Some(start))
        .await
        .unwrap();
    let mut tables = Vec::new();
    for _ in 0..6 {
        tables.push(next(&mut all).await.changes[0].table.clone());
    }
    assert_eq!(tables.last().map(String::as_str), Some("notes"));
    assert!(tables.iter().all(|t| !t.starts_with("engine_")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert reading a copy and following from its snapshot sees every later
/// change and nothing before it.
async fn follows_from_a_consistent_copy() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    run(
        &engine,
        "(plan (insert orders (values (columns a b) (row 1 1)) (columns id qty)))",
    )
    .await;
    let mut s = session(&engine, admin());
    s.begin(TxnOptions::default().read_only()).await.unwrap();
    let copied = s
        .query("(plan (get orders))", Vec::new())
        .await
        .unwrap()
        .row_count();
    let snapshot = s.snapshot_ts().unwrap();
    s.commit().await.unwrap();
    assert_eq!(copied, 1);
    let later = run(
        &engine,
        "(plan (insert orders (values (columns a b) (row 2 2)) (columns id qty)))",
    )
    .await
    .unwrap();
    let mut feed = engine
        .changes(ChangeFilter::tables(["orders"]), Some(snapshot))
        .await
        .unwrap();
    let batch = next(&mut feed).await;
    assert_eq!(batch.commit_ts, later);
    assert_eq!(batch.changes[0].key, vec![Datum::Int8(2)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert rows are decoded with the table's current columns across an added
/// column, and a dropped table ends the feed with `42P01`.
async fn schema_changes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let start = engine.visible_ts();
    run(
        &engine,
        "(plan (insert orders (values (columns a b) (row 1 1)) (columns id qty)))",
    )
    .await;
    let mut admin_session = session(&engine, admin());
    admin_session
        .add_column(
            "orders",
            &interlaken_engine::catalog::ColumnSpec::new("note", ty("(option text)")),
        )
        .await
        .unwrap();
    run(
        &engine,
        "(plan (insert orders (values (columns a b c) (row 2 2 \"n\")) (columns id qty note)))",
    )
    .await;
    let mut feed = engine
        .changes(ChangeFilter::tables(["orders"]), Some(start))
        .await
        .unwrap();
    let old = next(&mut feed).await;
    assert_eq!(
        old.changes[0].row,
        Some(vec![Datum::Int8(1), Datum::Int8(1), Datum::Null])
    );
    let new = next(&mut feed).await;
    assert_eq!(
        new.changes[0].row,
        Some(vec![
            Datum::Int8(2),
            Datum::Int8(2),
            Datum::Text("n".into())
        ])
    );

    admin_session.drop_table("orders").await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ended.unwrap_err().sqlstate(), "42P01");
    assert!(feed.next().await.is_none(), "the feed has ended");

    let missing = engine.changes(ChangeFilter::tables(["nope"]), None).await;
    assert_eq!(missing.unwrap_err().sqlstate(), "42P01");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a position older than the change log's floor ends the feed with
/// a gap (`55000`), and resyncing from a fresh snapshot works.
async fn gaps_end_the_feed() {
    let dir = tempfile::tempdir().unwrap();
    let small = EngineOptions {
        storage: StorageOptions {
            change_retention: ChangeRetention::Memory { bytes: 300 },
            ..options().storage
        },
        ..options()
    };
    let engine = Engine::create(dir.path(), small, &admin()).await.unwrap();
    session(&engine, admin())
        .create_table(&table(
            "orders",
            &[("id", "int8"), ("qty", "int8")],
            &["id"],
        ))
        .await
        .unwrap();
    let start = engine.visible_ts();
    for id in 0..50 {
        run(
            &engine,
            &format!("(plan (insert orders (values (columns a b) (row {id} 0)) (columns id qty)))"),
        )
        .await;
    }
    let mut feed = engine
        .changes(ChangeFilter::tables(["orders"]), Some(start))
        .await
        .unwrap();
    let error = feed.next().await.unwrap().unwrap_err();
    assert_eq!(error.sqlstate(), "55000", "{error}");
    assert!(feed.next().await.is_none());

    let mut s = session(&engine, admin());
    s.begin(TxnOptions::default().read_only()).await.unwrap();
    let snapshot = s.snapshot_ts().unwrap();
    s.commit().await.unwrap();
    let mut feed = engine
        .changes(ChangeFilter::tables(["orders"]), Some(snapshot))
        .await
        .unwrap();
    let later = run(
        &engine,
        "(plan (insert orders (values (columns a b) (row 99 0)) (columns id qty)))",
    )
    .await
    .unwrap();
    assert_eq!(next(&mut feed).await.commit_ts, later);
}
