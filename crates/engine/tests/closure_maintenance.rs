//! Closure maintenance in DML (milestone F2, ADR-0017, ADR-0032).
//!
//! DML on a membership edges table keeps the path-counted closure tables
//! up to date in the same statement, so authorization later in the same
//! transaction sees the new memberships.

#[path = "../../ir/tests/support/security.rs"]
mod security;
mod support;

use interlaken_ir::interp::Datum;
use interlaken_ir::security::EntityUid;
use support::session::{activate_rls, admin, create, install, replace_rows, session};

/// Return the rows of a closure table as sorted (descendant, ancestor, paths) triples.
async fn closure(s: &mut interlaken_engine::Session, table: &str) -> Vec<(i64, i64, i64)> {
    let rows = s
        .query(&format!("(plan (get {table}))"), Vec::new())
        .await
        .unwrap()
        .rows();
    let mut out: Vec<(i64, i64, i64)> = rows
        .iter()
        .map(|r| match (&r[0], &r[1], &r[2]) {
            (Datum::Int8(d), Datum::Int8(a), Datum::Int8(n)) => (*d, *a, *n),
            other => panic!("unexpected closure row {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

/// Return the SQLSTATE of a failed statement.
fn state<T: std::fmt::Debug>(r: Result<T, interlaken_engine::EngineError>) -> &'static str {
    r.unwrap_err().sqlstate()
}

#[tokio::test]
/// Assert inserts, updates and deletes of folder edges keep both folder
/// closures path-counted, across entity types.
async fn folder_edges_maintain_closures() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    install(&mut root, &security::catalog()).await;
    let run = |s: &'static str| s;
    // Folders 1 <- 2 <- 3 and 1 <- 4, document 10 in folder 3.
    for q in [
        "(plan (insert folder_parents (values (columns c p) (row 2 1) (row 3 2) (row 4 1)) (columns child parent)))",
        "(plan (insert doc_folders (values (columns d f) (row 10 3)) (columns doc folder)))",
    ] {
        root.query(run(q), Vec::new()).await.unwrap();
    }
    assert_eq!(
        closure(&mut root, "folder_closure").await,
        [(2, 1, 1), (3, 1, 1), (3, 2, 1), (4, 1, 1)]
    );
    assert_eq!(
        closure(&mut root, "doc_folders_closure").await,
        [(10, 1, 1), (10, 2, 1), (10, 3, 1)]
    );
    // A second path from 3 to 1 through 4: counts go to 2, and survive removing one path.
    root.query(
        "(plan (insert folder_parents (values (columns c p) (row 3 4)) (columns child parent)))",
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        closure(&mut root, "folder_closure").await,
        [(2, 1, 1), (3, 1, 2), (3, 2, 1), (3, 4, 1), (4, 1, 1)]
    );
    assert_eq!(
        closure(&mut root, "doc_folders_closure").await,
        [(10, 1, 2), (10, 2, 1), (10, 3, 1), (10, 4, 1)]
    );
    root.query(
        "(plan (delete folder_parents (filter (get folder_parents :as e) (and (= e.child 3) (= e.parent 2))) :as e))",
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        closure(&mut root, "folder_closure").await,
        [(2, 1, 1), (3, 1, 1), (3, 4, 1), (4, 1, 1)]
    );
    assert_eq!(
        closure(&mut root, "doc_folders_closure").await,
        [(10, 1, 1), (10, 3, 1), (10, 4, 1)]
    );
    // Moving document 10 from folder 3 to folder 2 is an update of its edge.
    root.query(
        "(plan (update doc_folders (filter (get doc_folders :as e) (= e.doc 10)) (set folder 2) :as e))",
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        closure(&mut root, "doc_folders_closure").await,
        [(10, 1, 1), (10, 2, 1)]
    );
    // A cycle fails the statement and changes nothing.
    let err = root
        .query(
            "(plan (insert folder_parents (values (columns c p) (row 1 3)) (columns child parent)))",
            Vec::new(),
        )
        .await;
    assert_eq!(state(err), "23514");
    let err = root
        .query(
            "(plan (insert folder_parents (values (columns c p) (row 5 5)) (columns child parent)))",
            Vec::new(),
        )
        .await;
    assert_eq!(state(err), "23514");
    assert_eq!(
        closure(&mut root, "folder_closure").await,
        [(2, 1, 1), (3, 1, 1), (3, 4, 1), (4, 1, 1)]
    );
    // Clients can't write closure tables directly.
    let err = root
        .query(
            "(plan (insert folder_closure (values (columns d a n) (row 7 8 1)) (columns descendant ancestor paths)))",
            Vec::new(),
        )
        .await;
    assert_eq!(state(err), "42501");
}

#[tokio::test]
/// Assert a membership added or removed by DML is seen by authorization
/// in the next statement of the same transaction, and undone with it.
async fn memberships_changed_by_dml_govern_the_same_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    install(&mut root, &security::catalog()).await;
    let db = security::sample().database();
    let catalog = security::catalog();
    let tables: Vec<(String, Vec<Vec<Datum>>)> = catalog
        .tables()
        .iter()
        .map(|t| (t.name.last().to_string(), db.rows(t.id).to_vec()))
        .collect();
    let tables: Vec<(&str, Vec<Vec<Datum>>)> = tables
        .iter()
        .map(|(n, r)| (n.as_str(), r.clone()))
        .collect();
    replace_rows(&engine, &tables).await;
    activate_rls(
        &mut root,
        "(policies (permit (read) Doc (in principal (entity Group 1))))",
    )
    .await
    .unwrap();
    // Bob (2) is in no group, so he sees nothing until he joins group 1.
    let mut bob = session(&engine, EntityUid::long("User", 2));
    let docs = "(plan (project (get docs :as d) d.id))";
    bob.begin(interlaken_storage::TxnOptions::default())
        .await
        .unwrap();
    assert!(bob.query(docs, Vec::new()).await.unwrap().rows().is_empty());
    bob.query(
        "(plan (insert user_groups (values (columns m g) (row 2 1)) (columns member grp)))",
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(bob.query(docs, Vec::new()).await.unwrap().row_count(), 4);
    bob.query(
        "(plan (delete user_groups (filter (get user_groups :as e) (= e.member 2)) :as e))",
        Vec::new(),
    )
    .await
    .unwrap();
    assert!(bob.query(docs, Vec::new()).await.unwrap().rows().is_empty());
    bob.query(
        "(plan (insert user_groups (values (columns m g) (row 2 1)) (columns member grp)))",
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(bob.query(docs, Vec::new()).await.unwrap().row_count(), 4);
    bob.rollback().unwrap();
    assert!(bob.query(docs, Vec::new()).await.unwrap().rows().is_empty());
}

/// Create the fixture with the sample data and `policies` active.
async fn governed(dir: &std::path::Path, policies: &str) -> interlaken_engine::Engine {
    let engine = create(dir).await;
    let mut root = session(&engine, admin());
    let catalog = security::catalog();
    install(&mut root, &catalog).await;
    let db = security::sample().database();
    let tables: Vec<(String, Vec<Vec<Datum>>)> = catalog
        .tables()
        .iter()
        .map(|t| (t.name.last().to_string(), db.rows(t.id).to_vec()))
        .collect();
    let tables: Vec<(&str, Vec<Vec<Datum>>)> = tables
        .iter()
        .map(|(n, r)| (n.as_str(), r.clone()))
        .collect();
    replace_rows(&engine, &tables).await;
    activate_rls(&mut root, policies).await.unwrap();
    engine
}

/// Write an ungoverned row, so the transaction has something to validate at commit.
async fn write_note(s: &mut interlaken_engine::Session, id: i64) {
    s.query(
        &format!("(plan (insert notes (values (columns i b) (row {id} \"n\")) (columns id body)))"),
        Vec::new(),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
/// Assert a membership removed by DML in a concurrent transaction fails a
/// SERIALIZABLE writer whose authorization relied on it, and only under
/// SERIALIZABLE, as for closure rows changed directly (ADR-0019).
async fn dml_revocations_conflict_under_serializable() {
    use interlaken_storage::{Isolation, TxnOptions};
    let docs = "(plan (project (get docs :as d) d.id))";
    for (isolation, conflicts) in [
        (Isolation::Serializable, true),
        (Isolation::RepeatableRead, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let engine = governed(
            dir.path(),
            "(policies (permit (read) Doc (in principal (entity Group 1))))",
        )
        .await;
        // Cy (3) is in group 1, so she reads every document.
        let mut cy = session(&engine, EntityUid::long("User", 3));
        cy.begin(TxnOptions::new(isolation)).await.unwrap();
        assert_eq!(cy.query(docs, Vec::new()).await.unwrap().row_count(), 4);
        write_note(&mut cy, 1).await;
        session(&engine, admin())
            .query(
                "(plan (delete user_groups (filter (get user_groups :as e) (= e.member 3)) :as e))",
                Vec::new(),
            )
            .await
            .unwrap();
        let result = cy.commit().await;
        if conflicts {
            assert_eq!(state(result), "40001", "{isolation:?}");
        } else {
            result.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
/// Assert concurrent edge changes that would together leave a closure
/// inconsistent conflict at every isolation level, and the survivor's
/// closure is exact.
async fn concurrent_edge_changes_conflict_at_every_level() {
    use interlaken_storage::{Isolation, TxnOptions};
    for isolation in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let engine = create(dir.path()).await;
        let mut root = session(&engine, admin());
        install(&mut root, &security::catalog()).await;
        // T1 puts folder 3 in folder 2 while T2 puts folder 2 in folder 1:
        // neither sees the other's edge, so neither would add (3, 1).
        let mut t1 = session(&engine, admin());
        let mut t2 = session(&engine, admin());
        t1.begin(TxnOptions::new(isolation)).await.unwrap();
        t2.begin(TxnOptions::new(isolation)).await.unwrap();
        t1.query(
            "(plan (insert folder_parents (values (columns c p) (row 3 2)) (columns child parent)))",
            Vec::new(),
        )
        .await
        .unwrap();
        t2.query(
            "(plan (insert folder_parents (values (columns c p) (row 2 1)) (columns child parent)))",
            Vec::new(),
        )
        .await
        .unwrap();
        t1.commit().await.unwrap();
        assert_eq!(state(t2.commit().await), "40001", "{isolation:?}");
        assert_eq!(closure(&mut root, "folder_closure").await, [(3, 2, 1)]);
        // Retried after the first committed, it sees the edge and adds (3, 1).
        root.query(
            "(plan (insert folder_parents (values (columns c p) (row 2 1)) (columns child parent)))",
            Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            closure(&mut root, "folder_closure").await,
            [(2, 1, 1), (3, 1, 1), (3, 2, 1)]
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
/// Assert policies read entities beyond the principal through the
/// transaction: a SERIALIZABLE writer whose authorization read its
/// manager's department fails if that department changes concurrently.
async fn entities_beyond_the_principal_join_the_read_set() {
    use interlaken_storage::{Isolation, TxnOptions};
    let dir = tempfile::tempdir().unwrap();
    // Ann (1) is managed by bob (2, ops); bob owns documents 11 and 12.
    let engine = governed(
        dir.path(),
        "(policies (permit (read) Doc (== resource.owner.dept principal.manager.dept)))",
    )
    .await;
    let docs = "(plan (sort (project (get docs :as d) d.id) (asc id)))";
    let mut ann = session(&engine, EntityUid::long("User", 1));
    assert_eq!(
        ann.query(docs, Vec::new()).await.unwrap().rows(),
        [vec![Datum::Int8(11)], vec![Datum::Int8(12)]]
    );
    ann.begin(TxnOptions::new(Isolation::Serializable))
        .await
        .unwrap();
    assert_eq!(ann.query(docs, Vec::new()).await.unwrap().row_count(), 2);
    write_note(&mut ann, 1).await;
    // Bob moves to engineering, behind ann's back.
    let mut users: Vec<Vec<Datum>> = security::sample().database().rows(security::USERS).to_vec();
    users[1][2] = Datum::Text("eng".into());
    replace_rows(&engine, &[("users", users)]).await;
    assert_eq!(state(ann.commit().await), "40001");
    // Afterwards ann's manager is in engineering, with ann's own document and his.
    assert_eq!(
        ann.query(docs, Vec::new()).await.unwrap().rows(),
        [
            vec![Datum::Int8(10)],
            vec![Datum::Int8(11)],
            vec![Datum::Int8(12)]
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not a check: run with --ignored --nocapture"]
/// Measure closure maintenance on a deep chain of folders and a wide fan of
/// documents (the cost `mvp-completion.md` asks F2 to measure).
async fn closure_maintenance_cost() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    install(&mut root, &security::catalog()).await;
    let depth = 200;
    let rows: Vec<String> = (2..=depth)
        .map(|f| format!("(row {f} {})", f - 1))
        .collect();
    let started = std::time::Instant::now();
    root.query(
        &format!(
            "(plan (insert folder_parents (values (columns c p) {}) (columns child parent)))",
            rows.join(" ")
        ),
        Vec::new(),
    )
    .await
    .unwrap();
    let chain = started.elapsed();
    let pairs = closure(&mut root, "folder_closure").await.len();
    let docs: Vec<String> = (1..=1000).map(|d| format!("(row {d} {depth})")).collect();
    let started = std::time::Instant::now();
    root.query(
        &format!(
            "(plan (insert doc_folders (values (columns d f) {}) (columns doc folder)))",
            docs.join(" ")
        ),
        Vec::new(),
    )
    .await
    .unwrap();
    let fan = started.elapsed();
    // One edge at the top of the chain now touches every folder and document below it.
    let started = std::time::Instant::now();
    root.query(
        "(plan (insert folder_parents (values (columns c p) (row 1 0)) (columns child parent)))",
        Vec::new(),
    )
    .await
    .unwrap();
    let top = started.elapsed();
    eprintln!(
        "chain of {depth} folders ({pairs} pairs): {chain:?}; 1000 documents at the bottom \
         ({} pairs): {fan:?}; one edge above all of them: {top:?}",
        1000 * (depth - 1)
    );
}

#[tokio::test]
/// Assert one statement can't run two writes that maintain the same
/// closure table, whose counts would overwrite each other's.
async fn two_maintaining_writes_in_one_statement_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    install(&mut root, &security::catalog()).await;
    let err = root
        .query(
            "(plan (insert doc_folders
               (project
                 (insert folder_parents (values (columns c p) (row 5 1)) (columns child parent) (returning child parent))
                 child parent)
               (columns doc folder)))",
            Vec::new(),
        )
        .await;
    assert_eq!(state(err), "0A000");
    assert!(closure(&mut root, "folder_closure").await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
/// Assert `in` on an entity beyond the principal loads that entity's
/// memberships: cy (3) is managed by ann (1), who is in group 2.
async fn memberships_of_entities_beyond_the_principal_are_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let engine = governed(
        dir.path(),
        "(policies (permit (read) Doc (in principal.manager (entity Group 2))))",
    )
    .await;
    let docs = "(plan (project (get docs :as d) d.id))";
    let mut cy = session(&engine, EntityUid::long("User", 3));
    assert_eq!(cy.query(docs, Vec::new()).await.unwrap().row_count(), 4);
    // Ann (managed by bob, who is in no group) sees nothing.
    let mut ann = session(&engine, EntityUid::long("User", 1));
    assert_eq!(ann.query(docs, Vec::new()).await.unwrap().row_count(), 0);
    // Ann leaving group 2 by DML takes effect for cy in the same transaction.
    cy.begin(interlaken_storage::TxnOptions::default())
        .await
        .unwrap();
    cy.query(
        "(plan (delete user_groups (filter (get user_groups :as e) (= e.member 1)) :as e))",
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(cy.query(docs, Vec::new()).await.unwrap().row_count(), 0);
    cy.rollback().unwrap();
}
