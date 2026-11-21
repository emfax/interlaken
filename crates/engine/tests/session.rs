//! The engine's public API (milestone F1, ADR-0031): DDL through the
//! engine, every column type, restarts, pinned plans and schema versions,
//! privileges, statement timeouts, transactions, `EXPLAIN` visibility and
//! concurrent DDL.

mod support;

use std::time::Duration;

use interlaken_engine::catalog::{
    AttributeSpec, ColumnSpec, EntitySpec, IndexKey, IndexSpec, MembershipSpec, load_catalog,
};
use interlaken_engine::{Engine, EngineError, Privilege, Session};
use interlaken_ir::catalog::Catalog;
use interlaken_ir::interp::Datum;
use interlaken_ir::security::EntityUid;
use interlaken_ir::{DataType, ScalarType};
use interlaken_storage::{Isolation, Storage, TxnOptions};

use support::session::{activate_rls, admin, create, options, replace_rows, session, table, ty};

/// Return the SQLSTATE of a failed result.
fn state<T: std::fmt::Debug>(result: Result<T, EngineError>) -> &'static str {
    result.unwrap_err().sqlstate()
}

/// Run `text` and return its rows.
async fn rows(session: &mut Session, text: &str) -> Vec<Vec<Datum>> {
    session
        .query(text, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}: {text}"))
        .rows()
}

/// Return user `n`.
fn user(n: i64) -> EntityUid {
    EntityUid::long("User", n)
}

/// Create users and documents governed by owner-only policies; return the admin session.
///
/// Users 1 (eng) and 2 (ops); documents 10 (owned by 1, secret 0) and 11
/// (owned by 2, secret 5).
async fn governed(engine: &Engine) -> Session {
    let mut root = session(engine, admin());
    root.create_table(&table(
        "users",
        &[("id", "int8"), ("dept", "(option text)")],
        &["id"],
    ))
    .await
    .unwrap();
    root.create_table(&table(
        "docs",
        &[
            ("id", "int8"),
            ("owner_id", "int8"),
            ("title", "(option text)"),
            ("secret", "(option int8)"),
        ],
        &["id"],
    ))
    .await
    .unwrap();
    root.declare_entity(&EntitySpec {
        name: "User".into(),
        table: "users".into(),
        key: "id".into(),
        attributes: vec![AttributeSpec::new("dept")],
    })
    .await
    .unwrap();
    root.declare_entity(&EntitySpec {
        name: "Doc".into(),
        table: "docs".into(),
        key: "id".into(),
        attributes: vec![
            AttributeSpec::reference("owner", "owner_id", "User"),
            AttributeSpec::new("title"),
            AttributeSpec::new("secret"),
        ],
    })
    .await
    .unwrap();
    let text = |s: &str| Datum::Text(s.into());
    replace_rows(
        engine,
        &[
            (
                "users",
                vec![
                    vec![Datum::Int8(1), text("eng")],
                    vec![Datum::Int8(2), text("ops")],
                ],
            ),
            (
                "docs",
                vec![
                    vec![
                        Datum::Int8(10),
                        Datum::Int8(1),
                        text("plan"),
                        Datum::Int8(0),
                    ],
                    vec![Datum::Int8(11), Datum::Int8(2), text("XX"), Datum::Int8(5)],
                ],
            ),
        ],
    )
    .await;
    // A new database's policy set is empty: governed tables show nothing.
    let ids = "(plan (project (get docs :as d) d.id))";
    assert!(rows(&mut session(engine, user(1)), ids).await.is_empty());
    activate_rls(
        &mut root,
        "(policies
           (permit (read insert update delete) Doc (== resource.owner principal))
           (permit (read) User true))",
    )
    .await
    .unwrap();
    root
}

#[tokio::test(flavor = "multi_thread")]
/// Assert tables of every column type are created through the engine,
/// written and read with IR text, indexed, and survive a restart with
/// their IR types intact.
async fn every_column_type_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let query = "(plan (sort (project (filter (get events :as e) (>= e.day (date \"2024-01-01\")))
                   e.id e.day e.at e.body e.tags e.score) (asc day)))";
    let expected = vec![vec![
        Datum::Uuid([7; 16]),
        Datum::Date(19_724),
        Datum::Timestamptz(1_704_164_645_000_000),
        Datum::Jsonb(r#"{"a": [1, 2]}"#.into()),
        Datum::Array(vec![Datum::Text("x".into()), Datum::Text("y".into())]),
        Datum::Float8(1.5),
    ]];
    {
        let engine = create(dir.path()).await;
        let mut root = session(&engine, admin());
        root.create_table(&table(
            "events",
            &[
                ("id", "uuid"),
                ("day", "(option date)"),
                ("at", "(option timestamptz)"),
                ("body", "(option jsonb)"),
                ("tags", "(option (array text))"),
                ("score", "(option float8)"),
            ],
            &["id"],
        ))
        .await
        .unwrap();
        root.create_index(&IndexSpec {
            name: "events_day".into(),
            table: "events".into(),
            columns: vec![IndexKey::desc("day")],
            unique: false,
        })
        .await
        .unwrap();
        let insert = "(plan (insert events
            (values (columns id day at body tags score)
              (row (uuid \"07070707-0707-0707-0707-070707070707\") (date \"2024-01-02\")
                   (timestamptz \"2024-01-02T03:04:05Z\") (param 1 (option jsonb)) (array \"x\" \"y\") 1.5)
              (row (uuid \"01010101-0101-0101-0101-010101010101\") (date \"2023-12-31\")
                   null null (cast (array) (array text)) null))
            (columns id day at body tags score)))";
        root.query(insert, vec![Datum::Jsonb(r#"{"a": [1, 2]}"#.into())])
            .await
            .unwrap();
        assert_eq!(rows(&mut root, query).await, expected);
        let plan = root.explain(query, false).await.unwrap();
        assert!(plan.contains("IndexScan events using events_day"), "{plan}");
    }
    let engine = Engine::open(dir.path(), options()).await.unwrap();
    let mut root = session(&engine, admin());
    assert_eq!(rows(&mut root, query).await, expected);
    let txn = engine.storage().begin(TxnOptions::default()).await.unwrap();
    let catalog = load_catalog(&txn).await.unwrap();
    let events = catalog.table_by_name(&"events".into()).unwrap();
    assert_eq!(
        events.column_by_name("tags").unwrap().ty,
        ty("(option (array text))")
    );
    assert_eq!(events.column_by_name("id").unwrap().ty, ty("uuid"));
    assert_eq!(events.indexes[0].name.as_str(), "events_day");
    assert!(events.indexes[0].columns[0].descending);
    // Two DDL statements, each bumping the version from 1.
    assert_eq!(catalog.schema_version().get(), 3);
}

#[tokio::test(flavor = "multi_thread")]
/// Assert arrays with plain elements widen to arrays with optional ones
/// (ADR-0034, backlog `array-element-nullability`), and not back.
async fn array_elements_widen_to_options() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    root.create_table(&table(
        "t",
        &[
            ("id", "int8"),
            ("loose", "(array (option text))"),
            ("strict", "(option (array text))"),
        ],
        &["id"],
    ))
    .await
    .unwrap();
    // A literal with no absent elements has type (array text), which widens.
    let insert = "(plan (insert t (values (columns id loose) (row 1 (array \"a\" \"b\"))
        (row 2 (array \"a\" null))) (columns id loose)))";
    root.query(insert, vec![]).await.unwrap();
    let text = |s: &str| Datum::Text(s.into());
    assert_eq!(
        rows(
            &mut root,
            "(plan (sort (project (get t) id loose) (asc id)))"
        )
        .await,
        vec![
            vec![Datum::Int8(1), Datum::Array(vec![text("a"), text("b")])],
            vec![Datum::Int8(2), Datum::Array(vec![text("a"), Datum::Null])],
        ]
    );
    // Optional elements don't narrow to plain ones.
    let narrow = "(plan (insert t (values (columns id loose strict)
        (row 3 (array \"a\") (array \"a\" null))) (columns id loose strict)))";
    let err = root.query(narrow, vec![]).await.unwrap_err();
    assert_eq!(err.sqlstate(), "42804", "{err}");
}

#[tokio::test(flavor = "multi_thread")]
/// Assert a plan pinned to a schema version runs until DDL changes the
/// version, then fails verification, and re-preparing it works.
async fn pinned_plans_go_stale_after_ddl() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    root.create_table(&table("t", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    root.query(
        "(plan (insert t (values (columns id) (row 1)) (columns id)))",
        vec![],
    )
    .await
    .unwrap();
    let text = "(plan (project (get t) id))";
    let pinned = root.prepare(text).await.unwrap();
    assert!(pinned.schema_version.is_some());
    assert_eq!(
        root.query_plan(&pinned, vec![]).await.unwrap().row_count(),
        1
    );
    root.add_column("t", &ColumnSpec::new("note", ty("(option text)")))
        .await
        .unwrap();
    let err = root.query_plan(&pinned, vec![]).await.unwrap_err();
    assert_eq!(err.sqlstate(), "0A000", "{err}");
    let fresh = root.prepare(text).await.unwrap();
    assert_eq!(
        root.query_plan(&fresh, vec![]).await.unwrap().row_count(),
        1
    );
    // The new column reads as NULL for the existing row.
    assert_eq!(
        rows(&mut root, "(plan (project (get t) note))").await,
        [vec![Datum::Null]]
    );
}

#[tokio::test(flavor = "multi_thread")]
/// Assert only administrators change the catalog, functions' leakproofness,
/// policies and privileges, and that grants and revocations take effect.
async fn privileged_operations_need_admin() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = governed(&engine).await;
    let mut alice = session(&engine, user(1));
    let spec = table("x", &[("id", "int8")], &["id"]);
    assert_eq!(state(alice.create_table(&spec).await), "42501");
    let text = [DataType::Scalar(ScalarType::Text)];
    assert_eq!(
        state(alice.set_function_leakproof("upper", &text, true).await),
        "42501"
    );
    assert_eq!(state(activate_rls(&mut alice, "(policies)").await), "42501");
    assert_eq!(
        state(alice.grant(&user(1), Privilege::Admin).await),
        "42501"
    );
    assert_eq!(state(alice.drop_table("docs").await), "42501");
    assert!(!alice.privileges().await.unwrap().admin);

    root.grant(&user(1), Privilege::Admin).await.unwrap();
    alice.create_table(&spec).await.unwrap();
    assert!(alice.privileges().await.unwrap().read_policies);
    assert!(root.revoke(&user(1), Privilege::Admin).await.unwrap());
    assert_eq!(state(alice.drop_table("x").await), "42501");
    assert!(!root.revoke(&user(1), Privilege::Admin).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
/// Assert clients can't see, write or create the engine's own tables, nor storage's.
async fn engine_tables_are_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    for query in [
        "(plan (get engine_policy_set))",
        "(plan (get engine_privileges))",
        "(plan (get sys_tables))",
        "(plan (delete engine_privileges (get engine_privileges)))",
    ] {
        let err = root.query(query, vec![]).await.unwrap_err();
        assert_eq!(err.sqlstate(), "42P01", "{query}: {err}");
    }
    let spec = table("engine_mine", &[("id", "int8")], &["id"]);
    assert_eq!(state(root.create_table(&spec).await), "42P16");
    assert_eq!(state(root.drop_table("engine_privileges").await), "42P16");
}

#[tokio::test(flavor = "multi_thread")]
/// Assert declarations are checked: a table backing an entity type can't
/// be dropped, duplicates and undeclared entity types are rejected, and a
/// rejected statement changes nothing.
async fn declarations_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = governed(&engine).await;
    assert_eq!(state(root.drop_table("docs").await), "42P16");
    let duplicate = EntitySpec {
        name: "User".into(),
        table: "users".into(),
        key: "id".into(),
        attributes: vec![],
    };
    assert_eq!(state(root.declare_entity(&duplicate).await), "42P16");
    root.create_table(&table(
        "edges",
        &[("a", "int8"), ("b", "int8")],
        &["a", "b"],
    ))
    .await
    .unwrap();
    root.create_table(&table(
        "closure",
        &[("a", "int8"), ("b", "int8"), ("paths", "int8")],
        &["a", "b"],
    ))
    .await
    .unwrap();
    let membership = |group: &str| MembershipSpec {
        member: "User".into(),
        group: group.into(),
        edges: "edges".into(),
        edge_columns: ("a".into(), "b".into()),
        closure: "closure".into(),
        closure_columns: ("a".into(), "b".into(), "paths".into()),
    };
    // Group is not a declared entity type: reloading the catalog rejects it.
    let err = root
        .declare_membership(&membership("Group"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Group"), "{err}");
    assert_eq!(err.sqlstate(), "42P16");
    // Column types must match the entity keys (int8 here) and paths must be int8.
    root.create_table(&table(
        "text_edges",
        &[("a", "text"), ("b", "int8")],
        &["a", "b"],
    ))
    .await
    .unwrap();
    root.create_table(&table(
        "int4_closure",
        &[("a", "int8"), ("b", "int8"), ("paths", "int4")],
        &["a", "b"],
    ))
    .await
    .unwrap();
    let err = root
        .declare_membership(&MembershipSpec {
            edges: "text_edges".into(),
            ..membership("Doc")
        })
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "42P16");
    assert!(
        err.to_string()
            .contains("member column text_edges.a has type text, but must have type int8"),
        "{err}"
    );
    let err = root
        .declare_membership(&MembershipSpec {
            closure: "int4_closure".into(),
            ..membership("Doc")
        })
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "42P16");
    assert!(err.to_string().contains("path count column"), "{err}");
    root.drop_table("text_edges").await.unwrap();
    root.drop_table("int4_closure").await.unwrap();
    let version = root
        .prepare("(plan (get edges))")
        .await
        .unwrap()
        .schema_version;
    root.declare_membership(&membership("Doc")).await.unwrap();
    assert_eq!(state(root.drop_entity("Doc").await), "42P16");
    assert_eq!(state(root.drop_table("closure").await), "42P16");
    root.drop_membership("User", "Doc").await.unwrap();
    root.drop_table("closure").await.unwrap();
    let after = root
        .prepare("(plan (get edges))")
        .await
        .unwrap()
        .schema_version;
    assert_eq!(
        after.map(|v| v.get()),
        version.map(|v| v.get() + 3),
        "the rejected declaration must not have bumped the schema version"
    );
}

#[tokio::test(flavor = "multi_thread")]
/// Assert `statement_timeout` cancels long statements, CPU-bound ones
/// included, undoing their writes and aborting an explicit transaction.
async fn statement_timeout_cancels() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    root.create_table(&table("nums", &[("n", "int8")], &["n"]))
        .await
        .unwrap();
    let values: Vec<String> = (1..=300).map(|n| format!("(row {n})")).collect();
    root.query(
        &format!(
            "(plan (insert nums (values (columns n) {}) (columns n)))",
            values.join(" ")
        ),
        vec![],
    )
    .await
    .unwrap();
    let count = "(plan (aggregate (get nums) (keys) (aggs (agg count))))";
    let huge = "(plan (aggregate (join inner (get nums :as a) (join inner (get nums :as b) (get nums :as c) true) true)
                  (keys) (aggs (agg count))))";
    root.set_statement_timeout(Some(Duration::from_millis(50)));
    let started = std::time::Instant::now();
    assert_eq!(state(root.query(huge, vec![]).await), "57014");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    // A cancelled write is undone.
    root.set_statement_timeout(Some(Duration::from_millis(1)));
    let copy = "(plan (insert nums (project (join inner (get nums :as a) (get nums :as b) true)
                  (+ (* a.n 1000) b.n)) (columns n)))";
    assert_eq!(state(root.query(copy, vec![]).await), "57014");
    root.set_statement_timeout(None);
    assert_eq!(rows(&mut root, count).await, [vec![Datum::Int8(300)]]);

    // In an explicit transaction, the timeout aborts it.
    root.begin(TxnOptions::default()).await.unwrap();
    root.query(
        "(plan (insert nums (values (columns n) (row 0)) (columns n)))",
        vec![],
    )
    .await
    .unwrap();
    root.set_statement_timeout(Some(Duration::from_millis(50)));
    assert_eq!(state(root.query(huge, vec![]).await), "57014");
    assert!(root.is_aborted().await.unwrap());
    root.set_statement_timeout(None);
    assert_eq!(state(root.query(count, vec![]).await), "25P02");
    root.commit().await.unwrap_err();
    assert!(!root.in_transaction());
    assert_eq!(rows(&mut root, count).await, [vec![Datum::Int8(300)]]);
}

#[tokio::test(flavor = "multi_thread")]
/// Assert savepoints undo statements, DDL included, within a transaction.
async fn savepoints_undo_ddl_and_dml() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    root.begin(TxnOptions::default()).await.unwrap();
    root.create_table(&table("a", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    let savepoint = root.savepoint().await.unwrap();
    root.create_table(&table("b", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    root.query(
        "(plan (insert a (values (columns id) (row 1)) (columns id)))",
        vec![],
    )
    .await
    .unwrap();
    // A failed statement aborts the transaction until the rollback.
    assert_eq!(
        state(root.query("(plan (get nope))", vec![]).await),
        "42P01"
    );
    assert!(root.is_aborted().await.unwrap());
    root.rollback_to(savepoint).await.unwrap();
    assert!(!root.is_aborted().await.unwrap());
    assert!(rows(&mut root, "(plan (get a))").await.is_empty());
    root.commit().await.unwrap();
    assert!(rows(&mut root, "(plan (get a))").await.is_empty());
    assert_eq!(state(root.query("(plan (get b))", vec![]).await), "42P01");
    assert_eq!(state(root.commit().await), "25P01");
}

#[tokio::test(flavor = "multi_thread")]
/// Assert `EXPLAIN` shows policy logic only to principals with `read-policies`.
async fn explain_visibility_follows_privileges() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = governed(&engine).await;
    let query = "(plan (project (get docs :as d) d.id))";
    let mut alice = session(&engine, user(1));
    let hidden = alice.explain(query, false).await.unwrap();
    assert!(
        hidden.contains("<policy>") && !hidden.contains("#1.1 = 1"),
        "{hidden}"
    );
    root.grant(&user(1), Privilege::ReadPolicies).await.unwrap();
    let shown = alice.explain(query, false).await.unwrap();
    assert!(!shown.contains("<policy>"), "{shown}");
    let analyzed = alice.explain(query, true).await.unwrap();
    assert!(analyzed.contains("rows=1"), "{analyzed}");
}

#[tokio::test(flavor = "multi_thread")]
/// Assert an administrator marking a function leakproof lets predicates
/// using it run below the policy barrier, in storage.
async fn leakproof_marking_moves_predicates_below_barriers() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = governed(&engine).await;
    root.grant(&user(1), Privilege::ReadPolicies).await.unwrap();
    let mut alice = session(&engine, user(1));
    let query = "(plan (project (filter (get docs :as d) (= (call upper d.title) \"PLAN\")) d.id))";
    let before = alice.explain(query, false).await.unwrap();
    assert!(!before.contains("pushed: (= (call"), "{before}");
    let text = [DataType::Scalar(ScalarType::Text)];
    root.set_function_leakproof("upper", &text, true)
        .await
        .unwrap();
    let after = alice.explain(query, false).await.unwrap();
    assert!(after.contains("pushed: (= (call"), "{after}");
    assert_eq!(rows(&mut alice, query).await, [vec![Datum::Int8(10)]]);
    let int = [DataType::Scalar(ScalarType::Int8)];
    assert_eq!(
        state(root.set_function_leakproof("upper", &int, true).await),
        "42P16"
    );
}

#[tokio::test(flavor = "multi_thread")]
/// Assert policies govern every principal, administrators included, and
/// write checks deny through the API without revealing rows.
async fn policies_govern_queries_and_writes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = governed(&engine).await;
    let mut alice = session(&engine, user(1));
    let ids = "(plan (sort (project (get docs :as d) d.id) (asc id)))";
    assert_eq!(rows(&mut alice, ids).await, [vec![Datum::Int8(10)]]);
    assert!(rows(&mut root, ids).await.is_empty());
    // Hidden rows can't raise errors.
    let divide = "(plan (project (filter (get docs :as d) (= (/ 100 (- d.secret 5)) -20)) d.id))";
    assert_eq!(rows(&mut alice, divide).await, [vec![Datum::Int8(10)]]);
    let steal = "(plan (insert docs (values (columns id owner_id title secret) (row 12 2 \"mine\" (cast null int8)))
                   (columns id owner_id title secret)))";
    let err = alice.query(steal, vec![]).await.unwrap_err();
    assert_eq!(err.sqlstate(), "42501");
    assert!(err.to_string().contains("docs"), "{err}");
    let own = "(plan (insert docs (values (columns id owner_id title secret) (row 12 1 \"mine\" (cast null int8)))
                 (columns id owner_id title secret)))";
    alice.query(own, vec![]).await.unwrap();
    assert_eq!(rows(&mut alice, ids).await.len(), 2);
    // A client can't smuggle in a policy filter of its own.
    let forged = "(plan (policy-filter (get docs :as d) true))";
    let err = alice.query(forged, vec![]).await.unwrap_err();
    assert_eq!(err.sqlstate(), "42501");
    let stats = engine.residual_cache_stats();
    assert!(stats.hits > 0, "{stats:?}");
}

#[tokio::test(flavor = "multi_thread")]
/// Assert DDL conflicts with concurrent writing transactions and other DDL,
/// so a plan never commits against a catalog that changed under it.
async fn concurrent_ddl_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    root.create_table(&table("t", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    let insert = |n: i64| format!("(plan (insert t (values (columns id) (row {n})) (columns id)))");

    for isolation in [
        Isolation::Serializable,
        Isolation::RepeatableRead,
        Isolation::ReadCommitted,
    ] {
        let mut writer = session(&engine, admin());
        writer.begin(TxnOptions::new(isolation)).await.unwrap();
        writer.query(&insert(1), vec![]).await.unwrap();
        writer
            .query("(plan (delete t (get t)))", vec![])
            .await
            .unwrap();
        // DDL on an unrelated table: only the catalog-wide schema version ties them.
        let mut ddl = session(&engine, admin());
        let name = format!("unrelated_{isolation:?}").to_lowercase();
        ddl.create_table(&table(&name, &[("id", "int8")], &["id"]))
            .await
            .unwrap();
        let err = writer.commit().await.unwrap_err();
        assert_eq!(err.sqlstate(), "40001", "{isolation:?}: {err}");
    }

    let mut a = session(&engine, admin());
    let mut b = session(&engine, admin());
    a.begin(TxnOptions::default()).await.unwrap();
    b.begin(TxnOptions::default()).await.unwrap();
    a.create_table(&table("a", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    b.create_table(&table("b", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    a.commit().await.unwrap();
    assert_eq!(state(b.commit().await), "40001");

    // A read-only query never conflicts with DDL.
    let mut reader = session(&engine, admin());
    reader
        .begin(TxnOptions {
            read_only: true,
            ..TxnOptions::default()
        })
        .await
        .unwrap();
    assert!(rows(&mut reader, "(plan (get t))").await.is_empty());
    root.drop_table("a").await.unwrap();
    assert!(rows(&mut reader, "(plan (get t))").await.is_empty());
    reader.commit().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
/// Assert a transaction's uncommitted catalog never reaches the shared
/// cache: another transaction may commit different DDL at the same
/// schema version.
async fn uncommitted_catalogs_are_not_shared() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut a = session(&engine, admin());
    a.begin(TxnOptions::default()).await.unwrap();
    a.create_table(&table("from_a", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    assert!(rows(&mut a, "(plan (get from_a))").await.is_empty());
    let mut b = session(&engine, admin());
    b.create_table(&table("from_b", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    a.rollback().unwrap();
    let mut c = session(&engine, admin());
    assert!(rows(&mut c, "(plan (get from_b))").await.is_empty());
    assert_eq!(state(c.query("(plan (get from_a))", vec![]).await), "42P01");
}

#[tokio::test(flavor = "multi_thread")]
/// Assert policies changed in a transaction apply to its later statements,
/// and its uncommitted residuals never reach the shared cache.
async fn uncommitted_policies_are_not_shared() {
    use interlaken_engine::policy::{PolicyDocument, PolicyLanguage};
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = governed(&engine).await;
    let rls = |body: &str| PolicyDocument {
        language: PolicyLanguage::Rls,
        body: body.into(),
    };
    let open = root
        .submit_policies(
            &rls("(policies (permit (read) Doc true) (permit (read) User true))"),
            "",
        )
        .await
        .unwrap();
    root.verify_policies(open).await.unwrap();
    let closed = root
        .submit_policies(&rls("(policies (permit (read) User true))"), "")
        .await
        .unwrap();
    root.verify_policies(closed).await.unwrap();
    let docs = "(plan (project (get docs :as d) d.id))";
    assert!(rows(&mut root, docs).await.is_empty());

    let mut a = session(&engine, admin());
    a.begin(TxnOptions::default()).await.unwrap();
    a.activate_policies(open, true).await.unwrap();
    assert_eq!(rows(&mut a, docs).await.len(), 2);
    let mut b = session(&engine, admin());
    b.activate_policies(closed, true).await.unwrap();
    a.rollback().unwrap();
    let mut c = session(&engine, admin());
    assert!(rows(&mut c, docs).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
/// Assert storage's transaction limits reach clients with their SQLSTATEs:
/// a write in a read-only transaction (`25006`), a write set over the
/// limit (`54000`) and a transaction past its maximum age (`25P04`).
async fn transaction_limits_have_their_sqlstates() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options();
    opts.storage.max_write_set_bytes = 8192;
    opts.storage.max_transaction_age = Duration::from_millis(200);
    let engine = Engine::create(dir.path(), opts, &admin()).await.unwrap();
    let mut root = session(&engine, admin());
    root.create_table(&table(
        "t",
        &[("id", "int8"), ("v", "(option text)")],
        &["id"],
    ))
    .await
    .unwrap();
    let insert = |n: usize| {
        let rows: Vec<String> = (0..n)
            .map(|i| format!("(row {i} \"{}\")", "x".repeat(100)))
            .collect();
        format!(
            "(plan (insert t (values (columns id v) {}) (columns id v)))",
            rows.join(" ")
        )
    };

    root.begin(TxnOptions::default().read_only()).await.unwrap();
    let err = root.query(&insert(1), Vec::new()).await.unwrap_err();
    assert_eq!(err.sqlstate(), "25006", "{err}");
    root.rollback().unwrap();

    let err = root.query(&insert(100), Vec::new()).await.unwrap_err();
    assert_eq!(err.sqlstate(), "54000", "{err}");

    root.begin(TxnOptions::default()).await.unwrap();
    root.query(&insert(1), Vec::new()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let err = root.query("(plan (get t))", Vec::new()).await.unwrap_err();
    assert_eq!(err.sqlstate(), "25P04", "{err}");
    root.rollback().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
/// Assert parameter values are checked against their declarations before
/// the statement runs: the count (08P01), each value's type (42804) and
/// NULL for a parameter whose type isn't optional (22004), for text and prepared plans, and
/// a rejected statement writes nothing.
async fn parameters_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    root.create_table(&table(
        "notes",
        &[("id", "int8"), ("body", "(option text)")],
        &["id"],
    ))
    .await
    .unwrap();
    let insert = "(plan (insert notes (values (columns id body)
        (row (param 1 int8) (param 2 (option text)))) (columns id body)))";
    let one = || vec![Datum::Int8(1), Datum::Text("a".into())];

    let count = |n: usize| one().into_iter().cycle().take(n).collect::<Vec<_>>();
    let err = root.query(insert, count(1)).await.unwrap_err();
    assert_eq!(err.sqlstate(), "08P01");
    assert_eq!(
        err.to_string(),
        "the statement has 2 parameters, but 1 value was supplied"
    );
    assert_eq!(state(root.query(insert, count(3)).await), "08P01");
    assert_eq!(state(root.query(insert, Vec::new()).await), "08P01");

    let err = root
        .query(insert, vec![Datum::Int4(1), Datum::Null])
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), "42804");
    assert!(
        err.to_string().contains("parameter 1 is declared int8"),
        "{err}"
    );
    assert_eq!(
        state(root.query(insert, vec![Datum::Null, Datum::Null]).await),
        "22004"
    );

    // A prepared plan is checked the same way, and so are streams.
    let plan = root.prepare(insert).await.unwrap();
    assert_eq!(
        state(
            root.query_plan(&plan, vec![Datum::Int8(1), Datum::Int8(2)])
                .await
        ),
        "42804"
    );
    assert_eq!(
        state(root.query_stream(insert, Vec::new()).await.map(|_| ())),
        "08P01"
    );
    // EXPLAIN ANALYZE runs without parameters, so it can't run this plan.
    assert_eq!(state(root.explain(insert, true).await), "08P01");
    assert!(rows(&mut root, "(plan (get notes))").await.is_empty());

    // Matching values run; a NULL fits the nullable parameter.
    root.query_plan(&plan, vec![Datum::Int8(1), Datum::Null])
        .await
        .unwrap();
    root.query(insert, vec![Datum::Int8(2), Datum::Text("b".into())])
        .await
        .unwrap();
    assert_eq!(rows(&mut root, "(plan (get notes))").await.len(), 2);

    // A failed check in an explicit transaction aborts it, like any failed statement.
    root.begin(TxnOptions::default()).await.unwrap();
    assert_eq!(state(root.query(insert, Vec::new()).await), "08P01");
    assert!(root.is_aborted().await.unwrap());
    root.rollback().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
/// Assert `admin` can't be revoked from the last principal holding it,
/// alone or by concurrent revocations at any isolation level, and that a
/// refused revocation changes nothing.
async fn the_last_admin_cant_be_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());

    let err = root.revoke(&admin(), Privilege::Admin).await.unwrap_err();
    assert_eq!(err.sqlstate(), "55000");
    assert!(err.to_string().contains("last administrator"), "{err}");
    assert!(root.privileges().await.unwrap().admin);

    // With a second administrator, either may go, but not both.
    root.grant(&user(1), Privilege::Admin).await.unwrap();
    root.grant(&user(1), Privilege::ReadPolicies).await.unwrap();
    assert!(root.revoke(&admin(), Privilege::Admin).await.unwrap());
    let mut alice = session(&engine, user(1));
    assert_eq!(
        state(alice.revoke(&user(1), Privilege::Admin).await),
        "55000"
    );
    // Revoking another privilege, or admin from a non-holder, is unaffected.
    assert!(
        alice
            .revoke(&user(1), Privilege::ReadPolicies)
            .await
            .unwrap()
    );
    assert!(!alice.revoke(&admin(), Privilege::Admin).await.unwrap());
    alice.grant(&admin(), Privilege::Admin).await.unwrap();

    // Two administrators revoking each other concurrently: each sees the
    // other still holding admin, but only one commit may succeed, even
    // under snapshot isolation, which doesn't check what was read.
    for isolation in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        let mut root = session(&engine, admin());
        root.begin(TxnOptions::new(isolation)).await.unwrap();
        alice.begin(TxnOptions::new(isolation)).await.unwrap();
        assert!(root.revoke(&user(1), Privilege::Admin).await.unwrap());
        assert!(alice.revoke(&admin(), Privilege::Admin).await.unwrap());
        root.commit().await.unwrap();
        assert_eq!(state(alice.commit().await), "40001", "{isolation:?}");
        assert!(root.privileges().await.unwrap().admin, "{isolation:?}");
        root.grant(&user(1), Privilege::Admin).await.unwrap();
    }
}
