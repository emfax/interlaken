//! Streaming results through a session (ADR-0031, backlog
//! `engine-streaming-results`): a [`QueryStream`] runs its statement as it
//! is read, under the statement's savepoint in the open transaction or in
//! an implicit one, and keeps the statement's semantics (ADR-0026) whether
//! it is read to the end, fails part way, times out or is dropped early.

mod support;

use std::time::Duration;

use interlaken_engine::{Engine, EngineError, Session};
use interlaken_ir::interp::Datum;
use interlaken_storage::encoding::Datum as Stored;
use interlaken_storage::{Storage, TxnOptions, catalog, rows};

use support::session::{admin, create, session, table};

/// Rows in `src`: enough for several batches.
const ROWS: i64 = 5_000;

/// Count the rows of `dst`.
const COUNT_DST: &str = "(plan (aggregate (get dst) (keys) (aggs (agg count))))";

/// Copy every row of `src` into `dst`, returning each copied value.
const COPY: &str = "(plan (insert dst (project (get src :as s) s.n) (columns n) (returning n)))";

/// Copy every row of `src` into `dst`, failing at the row with value 4,000:
/// after the first batches have been written and returned.
const COPY_FAILING: &str = "(plan (insert dst (project (get src :as s) (+ s.n (/ 0 (- 4000 s.n)))) (columns n) (returning n)))";

/// Create `src` holding `1..=ROWS` and an empty `dst`; return the engine's admin session.
async fn fixture(engine: &Engine) -> Session {
    let mut root = session(engine, admin());
    for name in ["src", "dst"] {
        root.create_table(&table(name, &[("n", "(not-null int8)")], &["n"]))
            .await
            .unwrap();
    }
    let values: Vec<String> = (1..=ROWS).map(|n| format!("(row {n})")).collect();
    root.query(
        &format!(
            "(plan (insert src (values (columns n) {}) (columns n)))",
            values.join(" ")
        ),
        Vec::new(),
    )
    .await
    .unwrap();
    root
}

/// Return the number of rows in `dst`, as a new session sees it.
async fn committed(engine: &Engine) -> i64 {
    count(&mut session(engine, admin())).await
}

/// Return the number of rows in `dst`, as `session` sees it.
async fn count(session: &mut Session) -> i64 {
    match session.query(COUNT_DST, Vec::new()).await.unwrap().rows()[0][0] {
        Datum::Int8(n) => n,
        ref other => panic!("count is {other:?}"),
    }
}

/// Return the SQLSTATE of a failed result.
fn state<T: std::fmt::Debug>(result: Result<T, EngineError>) -> &'static str {
    result.unwrap_err().sqlstate()
}

#[tokio::test(flavor = "multi_thread")]
/// Assert a stream reports its columns before running, yields the rows in
/// several batches, and commits an implicit transaction at its end.
async fn autocommit_stream_commits_at_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = fixture(&engine).await;
    let mut stream = root.query_stream(COPY, Vec::new()).await.unwrap();
    assert_eq!(stream.columns(), ["n"]);
    assert_eq!(stream.types().len(), 1);
    let (mut batches, mut rows) = (0, 0);
    while let Some(batch) = stream.next().await {
        let batch = batch.unwrap();
        batches += 1;
        rows += batch.num_rows();
        if batches == 1 {
            // Nothing commits before the stream ends.
            assert_eq!(committed(&engine).await, 0);
        }
    }
    assert!(
        stream.next().await.is_none(),
        "a finished stream stays finished"
    );
    drop(stream);
    assert!(batches > 1, "{batches} batches");
    assert_eq!(rows, usize::try_from(ROWS).unwrap());
    assert!(!root.in_transaction());
    assert_eq!(committed(&engine).await, ROWS);
}

#[tokio::test(flavor = "multi_thread")]
/// Assert dropping a stream early rolls back its implicit transaction,
/// and `finish` runs the rest of the statement and commits it.
async fn autocommit_stream_dropped_early_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = fixture(&engine).await;
    let mut stream = root.query_stream(COPY, Vec::new()).await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert!(first.num_rows() > 0);
    drop(stream);
    assert!(!root.in_transaction());
    assert_eq!(committed(&engine).await, 0);
    assert_eq!(count(&mut root).await, 0);

    let mut stream = root.query_stream(COPY, Vec::new()).await.unwrap();
    stream.next().await.unwrap().unwrap();
    stream.finish().await.unwrap();
    assert!(!root.in_transaction());
    assert_eq!(committed(&engine).await, ROWS);
}

#[tokio::test(flavor = "multi_thread")]
/// Assert an error part way through a stream rolls back its implicit
/// transaction, and in an explicit one undoes the statement and aborts
/// the transaction.
async fn errors_mid_stream_undo_the_statement() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = fixture(&engine).await;
    let mut stream = root.query_stream(COPY_FAILING, Vec::new()).await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert!(
        first.num_rows() > 0,
        "the failure comes after the first batch"
    );
    let err = loop {
        match stream.next().await {
            Some(Ok(_)) => {}
            Some(Err(e)) => break e,
            None => panic!("the copy should fail"),
        }
    };
    assert_eq!(err.sqlstate(), "22012", "{err}");
    assert!(stream.next().await.is_none(), "a failed stream ends");
    drop(stream);
    assert!(!root.in_transaction());
    assert_eq!(committed(&engine).await, 0);

    root.begin(TxnOptions::default()).await.unwrap();
    root.query(
        "(plan (insert dst (values (columns n) (row -1)) (columns n)))",
        Vec::new(),
    )
    .await
    .unwrap();
    let mut stream = root.query_stream(COPY_FAILING, Vec::new()).await.unwrap();
    stream.next().await.unwrap().unwrap();
    let err = stream.collect().await.unwrap_err();
    assert_eq!(err.sqlstate(), "22012", "{err}");
    assert!(root.is_aborted());
    assert_eq!(state(root.query(COUNT_DST, Vec::new()).await), "25P02");
    assert_eq!(
        state(root.query_stream(COUNT_DST, Vec::new()).await.map(|_| ())),
        "25P02"
    );
    root.commit().await.unwrap_err();
    assert_eq!(committed(&engine).await, 0);

    // Planning errors abort an explicit transaction too.
    root.begin(TxnOptions::default()).await.unwrap();
    assert_eq!(
        state(
            root.query_stream("(plan (get nope))", Vec::new())
                .await
                .map(|_| ())
        ),
        "42P01"
    );
    assert!(root.is_aborted());
    root.rollback().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
/// Assert dropping a stream early in an explicit transaction undoes only
/// that statement's writes and leaves the transaction usable.
async fn explicit_stream_dropped_early_undoes_its_writes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = fixture(&engine).await;
    root.begin(TxnOptions::default()).await.unwrap();
    root.query(
        "(plan (insert dst (values (columns n) (row -1)) (columns n)))",
        Vec::new(),
    )
    .await
    .unwrap();
    let mut stream = root.query_stream(COPY, Vec::new()).await.unwrap();
    stream.next().await.unwrap().unwrap();
    drop(stream);
    assert!(root.in_transaction());
    assert!(!root.is_aborted());
    assert_eq!(
        count(&mut root).await,
        1,
        "only the earlier statement's row stays"
    );
    // A finished stream keeps its writes in the transaction until it commits.
    root.query_stream(COPY, Vec::new())
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();
    assert!(root.in_transaction());
    assert_eq!(count(&mut root).await, ROWS + 1);
    assert_eq!(committed(&engine).await, 0);
    root.commit().await.unwrap();
    assert_eq!(committed(&engine).await, ROWS + 1);
}

#[tokio::test(flavor = "multi_thread")]
/// Assert `statement_timeout` cancels a stream, both while it computes
/// and when it is read after the deadline, undoing its writes and
/// aborting an explicit transaction.
async fn statement_timeout_cancels_streams() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = fixture(&engine).await;
    // A CPU-bound query: the executor's deadline stops it.
    let huge = "(plan (aggregate (join inner (get src :as a) (join inner (get src :as b) (get src :as c) true) true)
                  (keys) (aggs (agg count))))";
    root.set_statement_timeout(Some(Duration::from_millis(50)));
    let started = std::time::Instant::now();
    let mut stream = root.query_stream(huge, Vec::new()).await.unwrap();
    let err = stream.next().await.unwrap().unwrap_err();
    assert_eq!(err.sqlstate(), "57014", "{err}");
    drop(stream);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    // A stream read after its deadline fails, and its writes are undone.
    root.set_statement_timeout(Some(Duration::from_millis(200)));
    let mut stream = root.query_stream(COPY, Vec::new()).await.unwrap();
    stream.next().await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let err = stream.collect().await.unwrap_err();
    assert_eq!(err.sqlstate(), "57014", "{err}");
    assert!(!root.in_transaction());
    assert_eq!(committed(&engine).await, 0);

    // Batches computed before the deadline but read after it fail too: a
    // sort's later batches come from memory, so only the executor's own
    // deadline can stop them.
    let sorted = "(plan (sort (get src :as s) (desc s.n)))";
    let mut stream = root.query_stream(sorted, Vec::new()).await.unwrap();
    stream.next().await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let err = stream.next().await.unwrap().unwrap_err();
    assert_eq!(err.sqlstate(), "57014", "{err}");
    drop(stream);

    // In an explicit transaction the timeout aborts it.
    root.set_statement_timeout(None);
    root.begin(TxnOptions::default()).await.unwrap();
    root.set_statement_timeout(Some(Duration::from_millis(200)));
    let mut stream = root.query_stream(COPY, Vec::new()).await.unwrap();
    stream.next().await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let err = stream.next().await.unwrap().unwrap_err();
    assert_eq!(err.sqlstate(), "57014", "{err}");
    drop(stream);
    assert!(root.is_aborted());
    root.rollback().unwrap();
    root.set_statement_timeout(None);
    assert_eq!(committed(&engine).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
/// Assert a stream within its timeout runs to completion.
async fn streams_within_their_timeout_complete() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = fixture(&engine).await;
    root.set_statement_timeout(Some(Duration::from_secs(30)));
    let result = root
        .query_stream(COPY, Vec::new())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(result.row_count(), usize::try_from(ROWS).unwrap());
    assert_eq!(committed(&engine).await, ROWS);
}

#[tokio::test(flavor = "multi_thread")]
/// Assert the timeout stops a stream that spends its time in storage
/// scans returning no rows.
async fn statement_timeout_cancels_streams_waiting_on_storage() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    root.create_table(&table("big", &[("n", "(not-null int8)")], &["n"]))
        .await
        .unwrap();
    let txn = engine.storage().begin(TxnOptions::default()).await.unwrap();
    let def = catalog::table(&txn, "big").await.unwrap().unwrap();
    for chunk in (0..400_000).collect::<Vec<i64>>().chunks(4096) {
        let batch = chunk.iter().map(|&n| vec![Stored::Int8(n)]).collect();
        rows::insert_many(&txn, &def, batch).await.unwrap();
    }
    txn.commit().await.unwrap();
    // Storage rejects every row, so the scan returns no rows until it ends.
    let nothing = "(plan (filter (get big :as b) (< (* b.n 2) -1)))";
    // The second run times a warm scan.
    root.query(nothing, Vec::new()).await.unwrap();
    let started = std::time::Instant::now();
    assert_eq!(
        root.query(nothing, Vec::new()).await.unwrap().row_count(),
        0
    );
    let full = started.elapsed();
    root.set_statement_timeout(Some(full / 20));
    let mut stream = root.query_stream(nothing, Vec::new()).await.unwrap();
    let err = stream.next().await.unwrap().unwrap_err();
    assert_eq!(
        err.sqlstate(),
        "57014",
        "{err} (a full scan takes {full:?})"
    );
}
