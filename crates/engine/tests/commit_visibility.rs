//! Commit timestamps and waiting for visibility (ADR-0040 phase 1).

mod support;

use std::time::Duration;

use interlaken_engine::{CommitTs, Engine};
use interlaken_storage::{Freshness, TxnOptions};
use support::session::{admin, create, session, table};

/// Create a database with an empty table `t(id int8)`.
async fn database(dir: &std::path::Path) -> Engine {
    let engine = create(dir).await;
    session(&engine, admin())
        .create_table(&table("t", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    engine
}

/// Return an insert of row `id` into `t`.
fn insert(id: i64) -> String {
    format!("(plan (insert t (values (columns a) (row {id})) (columns id)))")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert commits return increasing timestamps, an empty transaction none,
/// and autocommit statements theirs in the result and the stream.
async fn commits_return_their_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut s = session(&engine, admin());

    s.begin(TxnOptions::default()).await.unwrap();
    s.query(&insert(1), Vec::new()).await.unwrap();
    let first = s.commit().await.unwrap().expect("a writing commit");
    assert_eq!(engine.visible_ts(), first);

    s.begin(TxnOptions::default()).await.unwrap();
    s.query("(plan (get t))", Vec::new()).await.unwrap();
    assert_eq!(s.commit().await.unwrap(), None, "nothing written");

    let inserted = s.query(&insert(2), Vec::new()).await.unwrap();
    let second = inserted.commit_ts.expect("an autocommit write");
    assert!(second > first);
    let read = s.query("(plan (get t))", Vec::new()).await.unwrap();
    assert_eq!(read.commit_ts, None);

    let third = {
        let mut stream = s.query_stream(&insert(3), Vec::new()).await.unwrap();
        assert_eq!(stream.commit_ts(), None, "not yet ended");
        while let Some(batch) = stream.next().await {
            batch.unwrap();
        }
        stream.commit_ts().expect("the stream's commit")
    };
    assert!(third > second);

    // Inside an explicit transaction, statements don't commit.
    s.begin(TxnOptions::default()).await.unwrap();
    assert_eq!(
        s.query(&insert(4), Vec::new()).await.unwrap().commit_ts,
        None
    );
    s.rollback().unwrap();
}

#[test]
/// Assert the text form round-trips and rejects anything else.
fn text_form_round_trips() {
    let ts = CommitTs::from_raw(42);
    assert_eq!(ts.to_string().parse::<CommitTs>().unwrap(), ts);
    for bad in ["", "-1", "4 2", "x", "18446744073709551616"] {
        assert!(bad.parse::<CommitTs>().is_err(), "{bad:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a transaction reports its snapshot, which is the last commit.
async fn transactions_report_their_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut s = session(&engine, admin());
    assert_eq!(s.snapshot_ts(), None);
    let committed = s.query(&insert(1), Vec::new()).await.unwrap().commit_ts;
    s.begin(TxnOptions::default().read_only()).await.unwrap();
    assert_eq!(s.snapshot_ts(), committed);
    s.commit().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert waiting resolves at once for a visible commit, when a later
/// commit arrives, and otherwise times out with `57014`.
async fn waiting_for_a_commit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let now = engine.visible_ts();
    engine
        .wait_visible(now, Duration::from_millis(10))
        .await
        .unwrap();

    let future = CommitTs::from_raw(u64::MAX);
    let error = engine
        .wait_visible(future, Duration::from_millis(50))
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), "57014");

    // A waiter for the next commit is woken by it.
    let waiter = {
        let engine = engine.clone();
        tokio::spawn(async move {
            let next = CommitTs::from_raw(now.into_raw() + 1);
            engine.wait_visible(next, Duration::from_secs(10)).await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    session(&engine, admin())
        .query(&insert(1), Vec::new())
        .await
        .unwrap();
    waiter.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a read-only transaction at `AtLeast` sees the commit, and one
/// waiting for a commit that never comes times out with the statement
/// timeout.
async fn transactions_wait_for_a_commit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut s = session(&engine, admin());
    let ts = s
        .query(&insert(1), Vec::new())
        .await
        .unwrap()
        .commit_ts
        .unwrap();
    s.begin(
        TxnOptions::default()
            .read_only()
            .freshness(Freshness::AtLeast(ts)),
    )
    .await
    .unwrap();
    assert_eq!(
        s.query("(plan (get t))", Vec::new())
            .await
            .unwrap()
            .row_count(),
        1
    );
    s.commit().await.unwrap();

    s.set_statement_timeout(Some(Duration::from_millis(50)));
    let never = TxnOptions::default()
        .read_only()
        .freshness(Freshness::AtLeast(CommitTs::from_raw(u64::MAX)));
    assert_eq!(s.begin(never).await.unwrap_err().sqlstate(), "57014");
}
