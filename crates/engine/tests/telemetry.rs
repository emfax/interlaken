//! Statement usage, metrics and spans (ADR-0039 phase 2).
//!
//! A test recorder and subscriber are installed once for this binary. Their
//! values are process-wide, so each test holds [`LOCK`] and compares
//! readings taken before and after the work it checks.

// Each test runs on its own runtime, and the lock only orders the tests.
#![allow(clippy::await_holding_lock)]

mod support;

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, PoisonError};

use interlaken_engine::{
    Attribution, Engine, EngineError, EngineOptions, METRICS, Session, Usage, describe_metrics,
};
use interlaken_ir::Ident;
use interlaken_ir::security::{EntityUid, SecValue, SecurityContext};
use interlaken_storage::TxnOptions;
use interlaken_telemetry::{
    TestRecorder, TestSubscriber, install_test_recorder, install_test_subscriber,
};

use support::session::{admin, create_single, session, table};

/// Serialises the tests, whose metrics are process-wide.
static LOCK: Mutex<()> = Mutex::new(());

/// A marker that table names and values carry, which must never reach a
/// label or span field.
const MARKER: &str = "secretmarker7f3a";

/// Take the lock and return the installed recorder and subscriber.
fn setup() -> (
    MutexGuard<'static, ()>,
    &'static TestRecorder,
    &'static TestSubscriber,
) {
    let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    (guard, install_test_recorder(), install_test_subscriber())
}

/// Create a database with a table `name` holding three rows, and return
/// the engine, an admin session and the insert's usage.
async fn seeded(dir: &std::path::Path, name: &str) -> (Engine, Session, Usage) {
    let engine = create_single(dir).await;
    let mut root = session(&engine, admin());
    root.create_table(&table(name, &[("id", "int8"), ("v", "text")], &["id"]))
        .await
        .unwrap();
    let insert = format!(
        "(plan (insert {name} (values (columns id v) (row 1 \"{MARKER}\") (row 2 \"bb\") \
         (row 3 \"cc\")) (columns id v)))"
    );
    let usage = root.query(&insert, Vec::new()).await.unwrap().usage;
    (engine, root, usage)
}

/// Return the statements counter for `kind` and `outcome`.
fn statements(recorder: &TestRecorder, kind: &str, outcome: &str) -> u64 {
    recorder.counter(
        "interlaken_statements_total",
        &[("kind", kind), ("outcome", outcome)],
    )
}

#[tokio::test]
/// Assert a query's usage counts what it scanned, returned and read, and
/// an insert's counts what it wrote.
async fn usage_counts_the_work() {
    let (_guard, _, _) = setup();
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut root, insert) = seeded(dir.path(), "t").await;
    assert!(insert.bytes_written > 0);
    assert!(insert.cpu > std::time::Duration::ZERO);
    assert!(insert.elapsed >= insert.cpu);

    let result = root.query("(plan (get t))", Vec::new()).await.unwrap();
    let usage = result.usage;
    assert_eq!(usage.rows_scanned, 3);
    assert_eq!(usage.rows_returned, 3);
    assert!(usage.bytes_read >= u64::try_from(MARKER.len()).unwrap() + 4);
    assert_eq!(usage.bytes_written, 0);
    // A sort needs memory; a plain scan may not.
    let sorted = root
        .query("(plan (sort (get t :as t) (desc t.v)))", Vec::new())
        .await
        .unwrap();
    assert!(sorted.usage.peak_memory > 0);
}

#[tokio::test]
/// Assert a transaction's usage is the sum of its statements'.
async fn transaction_usage_adds_up() {
    let (_guard, _, _) = setup();
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut root, _) = seeded(dir.path(), "t").await;
    root.begin(TxnOptions::default()).await.unwrap();
    let a = root
        .query("(plan (get t))", Vec::new())
        .await
        .unwrap()
        .usage;
    let b = root
        .query("(plan (get t))", Vec::new())
        .await
        .unwrap()
        .usage;
    root.commit().await.unwrap();
    let total = root.transaction_usage();
    assert_eq!(total.rows_scanned, a.rows_scanned + b.rows_scanned);
    assert_eq!(total.rows_returned, 6);
    assert_eq!(total.cpu, a.cpu + b.cpu);
    // A statement outside a transaction is its own.
    let c = root
        .query("(plan (get t))", Vec::new())
        .await
        .unwrap()
        .usage;
    assert_eq!(root.transaction_usage(), c);
}

#[tokio::test]
/// Assert a streamed statement reports its usage once it ends, and a
/// dropped one counts as cancelled.
async fn streamed_usage_and_cancellation() {
    let (_guard, recorder, _) = setup();
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut root, _) = seeded(dir.path(), "t").await;
    let mut rows = root
        .query_stream("(plan (get t))", Vec::new())
        .await
        .unwrap();
    assert_eq!(rows.usage(), None);
    while let Some(batch) = rows.next().await {
        batch.unwrap();
    }
    assert_eq!(rows.usage().unwrap().rows_returned, 3);
    drop(rows);

    let cancelled = statements(recorder, "select", "cancelled");
    let rows = root
        .query_stream("(plan (get t))", Vec::new())
        .await
        .unwrap();
    drop(rows);
    assert_eq!(statements(recorder, "select", "cancelled") - cancelled, 1);
}

#[tokio::test]
/// Assert statements, rows, bytes, phases and denials are recorded.
async fn statement_metrics_move() {
    let (_guard, recorder, _) = setup();
    let dir = tempfile::tempdir().unwrap();
    let ddl_before = statements(recorder, "ddl", "ok");
    let inserts_before = statements(recorder, "insert", "ok");
    let (engine, mut root, insert) = seeded(dir.path(), "t").await;
    assert_eq!(statements(recorder, "ddl", "ok") - ddl_before, 1);
    assert_eq!(statements(recorder, "insert", "ok") - inserts_before, 1);
    assert!(recorder.counter("interlaken_bytes_written_total", &[]) >= insert.bytes_written);

    let selects = statements(recorder, "select", "ok");
    let scanned = recorder.counter("interlaken_rows_scanned_total", &[]);
    let returned = recorder.counter("interlaken_rows_returned_total", &[]);
    let read = recorder.counter("interlaken_bytes_read_total", &[]);
    let phase = |p: &str| {
        recorder
            .histogram(
                "interlaken_statement_duration_seconds",
                &[("kind", "select"), ("phase", p)],
            )
            .len()
    };
    let (parses, plans, executes) = (phase("parse"), phase("plan"), phase("execute"));
    let usage = root
        .query("(plan (get t))", Vec::new())
        .await
        .unwrap()
        .usage;
    assert_eq!(statements(recorder, "select", "ok") - selects, 1);
    assert_eq!(
        recorder.counter("interlaken_rows_scanned_total", &[]) - scanned,
        usage.rows_scanned
    );
    assert_eq!(
        recorder.counter("interlaken_rows_returned_total", &[]) - returned,
        3
    );
    assert_eq!(
        recorder.counter("interlaken_bytes_read_total", &[]) - read,
        usage.bytes_read
    );
    assert_eq!(phase("parse") - parses, 1);
    assert_eq!(phase("plan") - plans, 1);
    assert_eq!(phase("execute") - executes, 1);

    // A syntax error fails before planning finishes.
    let unplanned = statements(recorder, "unplanned", "error");
    assert!(root.query("(plan (get", Vec::new()).await.is_err());
    assert_eq!(statements(recorder, "unplanned", "error") - unplanned, 1);

    // A principal without `admin` is refused DDL.
    let denials = recorder.counter("interlaken_authz_denials_total", &[("kind", "statement")]);
    let mut user = session(&engine, EntityUid::long("User", 7));
    let refused = user
        .create_table(&table("u", &[("id", "int8")], &["id"]))
        .await;
    assert!(matches!(refused, Err(EngineError::PermissionDenied(_))));
    assert_eq!(
        recorder.counter("interlaken_authz_denials_total", &[("kind", "statement")]) - denials,
        1
    );
}

#[tokio::test]
/// Assert a statement's span carries its kind, outcome and usage, with
/// parse, plan and execute spans.
async fn statement_spans_are_recorded() {
    let (_guard, _, subscriber) = setup();
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut root, _) = seeded(dir.path(), "t").await;
    let before = subscriber.spans("interlaken.statement").len();
    let phases = |name: &str| subscriber.spans(name).len();
    let (parses, plans, executes) = (
        phases("interlaken.parse"),
        phases("interlaken.plan"),
        phases("interlaken.execute"),
    );
    root.query("(plan (get t))", Vec::new()).await.unwrap();
    let spans = subscriber.spans("interlaken.statement");
    assert_eq!(spans.len() - before, 1);
    let span = spans.last().unwrap();
    assert_eq!(span.field("kind"), Some("select"));
    assert_eq!(span.field("outcome"), Some("ok"));
    assert_eq!(span.field("rows_returned"), Some("3"));
    assert_eq!(span.field("rows_scanned"), Some("3"));
    assert_eq!(phases("interlaken.parse") - parses, 1);
    assert_eq!(phases("interlaken.plan") - plans, 1);
    assert_eq!(phases("interlaken.execute") - executes, 1);
}

#[tokio::test]
/// Assert table names and values never reach a metric label or span field.
async fn no_data_in_labels_or_spans() {
    let (_guard, recorder, subscriber) = setup();
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut root, _) = seeded(dir.path(), MARKER).await;
    root.query(&format!("(plan (get {MARKER}))"), Vec::new())
        .await
        .unwrap();
    assert!(
        root.query(&format!("(plan (get {MARKER}_missing))"), Vec::new())
            .await
            .is_err()
    );
    let leaked: Vec<String> = recorder
        .labels()
        .into_iter()
        .chain(subscriber.field_values())
        .filter(|value| value.contains(MARKER))
        .collect();
    assert!(leaked.is_empty(), "data reached telemetry: {leaked:?}");
}

#[tokio::test]
/// Assert every engine and storage metric recorded is declared, and
/// `describe_metrics` describes them all.
async fn recorded_metrics_are_declared() {
    let (_guard, recorder, _) = setup();
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut root, _) = seeded(dir.path(), "t").await;
    root.query("(plan (get t))", Vec::new()).await.unwrap();
    let declared: BTreeSet<String> = METRICS
        .iter()
        .chain(interlaken_storage::telemetry::METRICS)
        .map(|m| m.name.to_owned())
        .collect();
    let undeclared: Vec<String> = recorder
        .names()
        .into_iter()
        .filter(|name| name.starts_with("interlaken_") && !declared.contains(name))
        .collect();
    assert!(undeclared.is_empty(), "undeclared metrics: {undeclared:?}");
    describe_metrics();
    assert!(declared.is_subset(&recorder.described()));
}

/// Create a database attributing usage by `attribution`, keeping at most
/// `limit` keys, with a table `t` of one row.
async fn attributed(
    dir: &std::path::Path,
    attribution: Attribution,
    limit: usize,
) -> (Engine, Session) {
    let options = EngineOptions {
        attribution,
        attribution_limit: limit,
        ..support::session::options()
    };
    let engine = Engine::create(dir, options, &admin()).await.unwrap();
    let mut root = session(&engine, admin());
    root.create_table(&table("t", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    root.query(
        "(plan (insert t (values (columns id) (row 1)) (columns id)))",
        Vec::new(),
    )
    .await
    .unwrap();
    (engine, root)
}

/// Return the attributed statements counter for `key`.
fn attributed_statements(recorder: &TestRecorder, key: &str) -> u64 {
    recorder.counter("interlaken_usage_statements_total", &[("attribution", key)])
}

#[tokio::test]
/// Assert usage is attributed to principals, up to the limit of keys,
/// beyond which sessions count as `other`.
async fn usage_is_attributed_by_principal() {
    let (_guard, recorder, _) = setup();
    let dir = tempfile::tempdir().unwrap();
    let admin_key = admin().to_string();
    let admin_before = attributed_statements(recorder, &admin_key);
    let (engine, mut root) = attributed(dir.path(), Attribution::Principal, 2).await;
    root.query("(plan (get t))", Vec::new()).await.unwrap();
    // The insert and the query.
    assert_eq!(
        attributed_statements(recorder, &admin_key) - admin_before,
        2
    );
    assert!(
        recorder.counter(
            "interlaken_usage_rows_scanned_total",
            &[("attribution", admin_key.as_str())]
        ) >= 1
    );

    let user = |n: i64| EntityUid::long("User", n);
    let overflow = recorder.counter("interlaken_usage_attribution_overflow_total", &[]);
    let others = attributed_statements(recorder, "other");
    let second_before = attributed_statements(recorder, &user(1).to_string());
    let mut second = session(&engine, user(1));
    let mut third = session(&engine, user(2));
    second.query("(plan (get t))", Vec::new()).await.unwrap();
    third.query("(plan (get t))", Vec::new()).await.unwrap();
    assert_eq!(
        attributed_statements(recorder, &user(1).to_string()) - second_before,
        1
    );
    assert_eq!(attributed_statements(recorder, "other") - others, 1);
    assert_eq!(
        recorder.counter("interlaken_usage_attribution_overflow_total", &[]) - overflow,
        1
    );
}

#[tokio::test]
/// Assert usage is attributed to an application context field, with
/// sessions lacking it counted as `none`.
async fn usage_is_attributed_by_context() {
    let (_guard, recorder, _) = setup();
    let dir = tempfile::tempdir().unwrap();
    let (engine, _root) = attributed(dir.path(), Attribution::Context("tenant".into()), 10).await;
    let acme = attributed_statements(recorder, "acme-7f3a");
    let none = attributed_statements(recorder, "none");
    let mut context = SecurityContext::new(admin());
    context
        .application
        .push((Ident::new("tenant"), SecValue::Str("acme-7f3a".into())));
    let mut tenant = engine.session(context);
    tenant.query("(plan (get t))", Vec::new()).await.unwrap();
    let mut plain = session(&engine, admin());
    plain.query("(plan (get t))", Vec::new()).await.unwrap();
    assert_eq!(attributed_statements(recorder, "acme-7f3a") - acme, 1);
    assert_eq!(attributed_statements(recorder, "none") - none, 1);
}

#[tokio::test]
/// Assert nothing is attributed with attribution off.
async fn attribution_is_off_by_default() {
    let (_guard, recorder, _) = setup();
    let dir = tempfile::tempdir().unwrap();
    let before = recorder.series_count("interlaken_usage_statements_total");
    let (_engine, mut root, _) = seeded(dir.path(), "t").await;
    root.query("(plan (get t))", Vec::new()).await.unwrap();
    assert_eq!(
        recorder.series_count("interlaken_usage_statements_total"),
        before
    );
}
