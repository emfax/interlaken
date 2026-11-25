//! Subscriptions filtered by policies (ADR-0040 phase 4).

mod support;

use std::time::Duration;

use interlaken_engine::catalog::{AttributeSpec, EntitySpec};
use interlaken_engine::{ChangeBatch, ChangeKind, Engine, Subscription};
use interlaken_ir::interp::Datum;
use interlaken_ir::security::EntityUid;
use support::session::{activate_rls, admin, create, session, table};

/// Owners read their documents, with titles redacted unless marked public;
/// administrators do anything.
const POLICIES: &str = r#"(policies
  (permit (read insert update delete) Doc (is principal Admin))
  (permit (read) User true)
  (permit (read) Doc (== resource.owner principal))
  (permit (read) Doc (== resource.public true) :column title :mask redact)
  (permit (read) Doc (is principal Admin) :column title :mask redact))"#;

/// Create users 1 and 2 and the governed `docs` table.
async fn database(dir: &std::path::Path) -> Engine {
    let engine = create(dir).await;
    let mut root = session(&engine, admin());
    root.create_table(&table("users", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    root.create_table(&table(
        "docs",
        &[
            ("id", "int8"),
            ("owner", "int8"),
            ("title", "text"),
            ("public", "bool"),
        ],
        &["id"],
    ))
    .await
    .unwrap();
    root.query(
        "(plan (insert users (values (columns a) (row 1) (row 2)) (columns id)))",
        Vec::new(),
    )
    .await
    .unwrap();
    root.declare_entity(&EntitySpec {
        name: "User".into(),
        table: "users".into(),
        key: "id".into(),
        attributes: Vec::new(),
    })
    .await
    .unwrap();
    root.declare_entity(&EntitySpec {
        name: "Doc".into(),
        table: "docs".into(),
        key: "id".into(),
        attributes: vec![
            AttributeSpec::reference("owner", "owner", "User"),
            AttributeSpec::new("title"),
            AttributeSpec::new("public"),
        ],
    })
    .await
    .unwrap();
    activate_rls(&mut root, POLICIES).await.unwrap();
    engine
}

/// Run `statement` as the administrator.
async fn run(engine: &Engine, statement: &str) {
    session(engine, admin())
        .query(statement, Vec::new())
        .await
        .unwrap();
}

/// Return the next batch within 5 seconds.
async fn next(subscription: &mut Subscription) -> ChangeBatch {
    tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .expect("a batch within 5 seconds")
        .expect("the subscription continues")
        .unwrap()
}

/// Return a batch's changes as (kind, id, title).
fn summary(batch: &ChangeBatch) -> Vec<(ChangeKind, Datum, Option<Datum>)> {
    batch
        .changes
        .iter()
        .map(|c| {
            (
                c.kind,
                c.key[0].clone(),
                c.row.as_ref().map(|r| r[2].clone()),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a principal's subscription shows only rows it may read, with
/// masks, and that rows arrive and leave as access changes.
async fn subscriptions_follow_policies() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut ann = session(&engine, EntityUid::long("User", 1))
        .subscribe("docs", None)
        .await
        .unwrap();

    // Ann's own document, and bob's, which she never sees.
    run(
        &engine,
        r#"(plan (insert docs (values (columns a b c d) (row 10 1 "mine" false) (row 20 2 "bob's" false)) (columns id owner title public)))"#,
    )
    .await;
    let batch = next(&mut ann).await;
    assert_eq!(
        summary(&batch),
        [(
            ChangeKind::Insert,
            Datum::Int8(10),
            Some(Datum::Text("****".into()))
        )],
        "only her document, its title redacted"
    );

    // Bob's document changes: nothing for ann. Then hers becomes public.
    run(
        &engine,
        r#"(plan (update docs (filter (get docs :as d) (= d.id 20)) (set title "still bob's")))"#,
    )
    .await;
    run(
        &engine,
        "(plan (update docs (filter (get docs :as d) (= d.id 10)) (set public true)))",
    )
    .await;
    let batch = next(&mut ann).await;
    assert_eq!(
        summary(&batch),
        [(
            ChangeKind::Update,
            Datum::Int8(10),
            Some(Datum::Text("mine".into()))
        )]
    );

    // Bob's document is given to ann: it arrives as an insert. Hers is
    // given to bob: it leaves as a delete.
    run(
        &engine,
        "(plan (update docs (filter (get docs :as d) (= d.id 20)) (set owner 1)))",
    )
    .await;
    assert_eq!(next(&mut ann).await.changes[0].kind, ChangeKind::Insert);
    run(
        &engine,
        "(plan (update docs (filter (get docs :as d) (= d.id 10)) (set owner 2)))",
    )
    .await;
    let batch = next(&mut ann).await;
    assert_eq!(
        summary(&batch),
        [(ChangeKind::Delete, Datum::Int8(10), None)]
    );

    // A delete of a document she can read.
    run(
        &engine,
        "(plan (delete docs (filter (get docs :as d) (= d.id 20))))",
    )
    .await;
    let batch = next(&mut ann).await;
    assert_eq!(
        summary(&batch),
        [(ChangeKind::Delete, Datum::Int8(20), None)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a subscription resumes from a position, and refuses an unknown table.
async fn subscriptions_resume() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let start = engine.visible_ts();
    for id in 0..3 {
        run(
            &engine,
            &format!(
                r#"(plan (insert docs (values (columns a b c d) (row {id} 1 "t" true)) (columns id owner title public)))"#
            ),
        )
        .await;
    }
    let ann = session(&engine, EntityUid::long("User", 1));
    let mut all = ann.subscribe("docs", Some(start)).await.unwrap();
    let mut stamps = Vec::new();
    for _ in 0..3 {
        stamps.push(next(&mut all).await.commit_ts);
    }
    let mut resumed = ann.subscribe("docs", Some(stamps[0])).await.unwrap();
    assert_eq!(next(&mut resumed).await.commit_ts, stamps[1]);
    assert_eq!(resumed.position(), stamps[1]);
    assert_eq!(
        ann.subscribe("nope", None).await.unwrap_err().sqlstate(),
        "42P01"
    );
}
