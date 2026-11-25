//! Namespaces, the search path and owners (ADR-0042 phases 2 and 3).

mod support;

use interlaken_engine::Engine;
use interlaken_engine::catalog::{ForeignKeySpec, IndexKey, IndexSpec};
use interlaken_ir::interp::Datum;
use interlaken_ir::security::EntityUid;
use support::session::{admin, create, session, table};

/// Return user `n`.
fn user(n: i64) -> EntityUid {
    EntityUid::long("User", n)
}

/// Return the single `int8` of a one-row, one-column result.
async fn one(engine: &Engine, path: &[&str], statement: &str) -> i64 {
    let mut s = session(engine, admin());
    s.set_search_path(path.iter().copied());
    match s.query(statement, Vec::new()).await.unwrap().rows()[..] {
        [ref row] => match row[0] {
            Datum::Int8(n) => n,
            ref other => panic!("not an int8: {other:?}"),
        },
        ref rows => panic!("expected one row, got {rows:?}"),
    }
}

/// Create namespace `app`, owned by user 1, and table `todos(id, n)` in
/// both `public` and `app`, holding 1 and 2 respectively.
async fn database(dir: &std::path::Path) -> Engine {
    let engine = create(dir).await;
    let mut root = session(&engine, admin());
    root.create_namespace("app", &user(1)).await.unwrap();
    let todos = |name: &str| table(name, &[("id", "int8"), ("n", "int8")], &["id"]);
    root.create_table(&todos("todos")).await.unwrap();
    root.create_table(&todos("app.todos")).await.unwrap();
    root.query(
        "(plan (insert todos (values (columns a b) (row 1 1)) (columns id n)))",
        Vec::new(),
    )
    .await
    .unwrap();
    root.query(
        "(plan (insert app.todos (values (columns a b) (row 1 2)) (columns id n)))",
        Vec::new(),
    )
    .await
    .unwrap();
    engine
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert names resolve through the search path, qualified names exactly,
/// and new objects go in the first namespace of the path.
async fn names_resolve_through_the_search_path() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let query = "(plan (project (get todos :as t) t.n))";
    assert_eq!(one(&engine, &["public"], query).await, 1);
    assert_eq!(one(&engine, &["app", "public"], query).await, 2);
    assert_eq!(
        one(&engine, &["nope", "app"], query).await,
        2,
        "missing namespaces are skipped"
    );
    assert_eq!(
        one(
            &engine,
            &["app"],
            "(plan (project (get public.todos :as t) t.n))"
        )
        .await,
        1
    );
    assert_eq!(
        one(
            &engine,
            &["public"],
            "(plan (project (get app.todos :as t) t.n))"
        )
        .await,
        2
    );

    // New tables go in the first namespace that exists.
    let mut s = session(&engine, admin());
    s.set_search_path(["nope", "app"]);
    s.create_table(&table("notes", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    assert_eq!(
        session(&engine, admin())
            .query("(plan (get app.notes))", Vec::new())
            .await
            .unwrap()
            .row_count(),
        0
    );
    let error = session(&engine, admin())
        .query("(plan (get notes))", Vec::new())
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P01", "not in public");

    // A prepared plan keeps the table it resolved.
    let mut s = session(&engine, admin());
    let plan = s.prepare(query).await.unwrap();
    s.set_search_path(["app"]);
    let rows = s.query_plan(&plan, Vec::new()).await.unwrap().rows();
    assert_eq!(rows, vec![vec![Datum::Int8(1)]]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert an owner changes its namespace's schema without `admin`, but not
/// other namespaces', can give its namespace away, and can't make or drop
/// namespaces.
async fn owners_change_their_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut owner = session(&engine, user(1));
    owner.set_search_path(["app"]);
    owner
        .create_table(&table(
            "items",
            &[("id", "int8"), ("todo", "int8")],
            &["id"],
        ))
        .await
        .unwrap();
    owner
        .create_index(&IndexSpec {
            name: "items_by_todo".into(),
            table: "items".into(),
            columns: vec![IndexKey::asc("todo")],
            unique: false,
            nones_distinct: true,
        })
        .await
        .unwrap();
    owner.drop_index("items_by_todo").await.unwrap();
    let denied = owner
        .create_table(&table("public.mine", &[("id", "int8")], &["id"]))
        .await
        .unwrap_err();
    assert_eq!(denied.sqlstate(), "42501");
    assert_eq!(
        owner
            .create_namespace("other", &user(1))
            .await
            .unwrap_err()
            .sqlstate(),
        "42501"
    );
    assert_eq!(
        owner
            .drop_namespace("app", true)
            .await
            .unwrap_err()
            .sqlstate(),
        "42501"
    );
    // A foreign key into public needs public's owner too.
    let cross = ForeignKeySpec::new("items_todo", "items", ["todo"], "public.todos", ["id"]);
    assert_eq!(
        owner
            .create_foreign_key(&cross)
            .await
            .unwrap_err()
            .sqlstate(),
        "42501"
    );
    owner.set_namespace_owner("app", &user(2)).await.unwrap();
    assert_eq!(
        owner.drop_table("items").await.unwrap_err().sqlstate(),
        "42501",
        "no longer the owner"
    );
    let mut next = session(&engine, user(2));
    next.drop_table("app.items").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a namespace drops only when empty, or with cascade, which also
/// drops other namespaces' foreign keys into it.
async fn dropping_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut root = session(&engine, admin());
    root.create_table(&table(
        "refs",
        &[("id", "int8"), ("todo", "int8")],
        &["todo", "id"],
    ))
    .await
    .unwrap();
    root.create_foreign_key(&ForeignKeySpec::new(
        "refs_todo",
        "refs",
        ["todo"],
        "app.todos",
        ["id"],
    ))
    .await
    .unwrap();
    assert_eq!(
        root.drop_namespace("app", false)
            .await
            .unwrap_err()
            .sqlstate(),
        "42P16"
    );
    assert_eq!(
        root.drop_namespace("public", true)
            .await
            .unwrap_err()
            .sqlstate(),
        "42P16"
    );
    root.drop_namespace("app", true).await.unwrap();
    assert_eq!(
        root.query("(plan (get app.todos))", Vec::new())
            .await
            .unwrap_err()
            .sqlstate(),
        "42P01"
    );
    // The foreign key went with it: refs takes any value now.
    root.query(
        "(plan (insert refs (values (columns a b) (row 1 99)) (columns id todo)))",
        Vec::new(),
    )
    .await
    .unwrap();
    // The name is free again.
    root.create_namespace("app", &user(1)).await.unwrap();
    assert_eq!(
        root.create_namespace("app", &user(1))
            .await
            .unwrap_err()
            .sqlstate(),
        "42P16"
    );
    assert_eq!(
        root.create_namespace("engine_x", &user(1))
            .await
            .unwrap_err()
            .sqlstate(),
        "42P16"
    );
}
