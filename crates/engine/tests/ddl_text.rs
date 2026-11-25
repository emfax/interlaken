//! DDL as text (ADR-0043): statements through `Session::query`, scripts,
//! and the schema printed as a script that recreates it.

mod support;

use interlaken_engine::catalog::{
    AttributeSpec, ColumnSpec, EntitySpec, ForeignKeySpec, IndexKey, IndexSpec, MembershipSpec,
    ReferentialAction,
};
use interlaken_engine::{Engine, EngineError, Session};
use interlaken_ir::interp::Datum;
use interlaken_ir::security::EntityUid;
use interlaken_ir::text::print_type;
use support::session::{admin, create, session, table, ty};

/// A schema using every kind of object, as a script.
const SCHEMA: &str = r#"
; Namespaces and principal types first.
(create-namespace app :owner (entity User 1))
(declare-principal-type Service)
(declare-principal-type app.Robot)

(create-table users (column id int8) (column name (option text)) (primary-key id))
(create-table groups (column id int8) (primary-key id))
(create-table user_groups (column member int8) (column grp int8) (primary-key member grp))
(create-table user_group_closure
  (column descendant int8) (column ancestor int8) (column paths int8)
  (primary-key descendant ancestor))
(create-table app.todos
  (column id int8) (column owner int8) (column title text) (column done bool)
  (primary-key id))
(add-column app.todos note (option text))
(create-index todos_by_owner :table app.todos owner (desc done :nulls last))
(create-index users_name :table users name :unique true :nones-distinct false)

(declare-entity User :table users :key id (attr name))
(declare-entity Group :table groups :key id)
(declare-entity app.Todo :table app.todos :key id (attr owner :references User) (attr label title))
(declare-membership User Group :edges user_groups (member grp)
                               :closure user_group_closure (descendant ancestor paths))
(create-foreign-key todos_owner :table app.todos (columns owner) (references users id)
                    :on-delete cascade)
(set-leakproof lower (text) true)
"#;

/// Return a new database and an administrator's session on it.
async fn database(dir: &std::path::Path) -> (Engine, Session) {
    let engine = create(dir).await;
    let root = session(&engine, admin());
    (engine, root)
}

/// Return a description of the user catalog that ignores IDs and order.
async fn catalog_shape(root: &mut Session) -> Vec<String> {
    root.begin(Default::default()).await.unwrap();
    let text = root.schema_text().await.unwrap();
    root.rollback().unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    lines.sort();
    lines
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert each DDL form gives the same catalog as its Rust call.
async fn text_and_rust_agree() {
    let text_dir = tempfile::tempdir().unwrap();
    let (_text_engine, mut by_text) = database(text_dir.path()).await;
    by_text.execute_script(SCHEMA).await.unwrap();

    let rust_dir = tempfile::tempdir().unwrap();
    let (_rust_engine, mut by_rust) = database(rust_dir.path()).await;
    let r = &mut by_rust;
    r.create_namespace("app", &EntityUid::long("User", 1))
        .await
        .unwrap();
    r.declare_principal_type("Service").await.unwrap();
    r.declare_principal_type("app.Robot").await.unwrap();
    let mut users = table("users", &[("id", "int8")], &["id"]);
    users
        .columns
        .push(ColumnSpec::new("name", ty("(option text)")));
    r.create_table(&users).await.unwrap();
    r.create_table(&table("groups", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    r.create_table(&table(
        "user_groups",
        &[("member", "int8"), ("grp", "int8")],
        &["member", "grp"],
    ))
    .await
    .unwrap();
    r.create_table(&table(
        "user_group_closure",
        &[
            ("descendant", "int8"),
            ("ancestor", "int8"),
            ("paths", "int8"),
        ],
        &["descendant", "ancestor"],
    ))
    .await
    .unwrap();
    r.create_table(&table(
        "app.todos",
        &[
            ("id", "int8"),
            ("owner", "int8"),
            ("title", "text"),
            ("done", "bool"),
        ],
        &["id"],
    ))
    .await
    .unwrap();
    r.add_column("app.todos", &ColumnSpec::new("note", ty("(option text)")))
        .await
        .unwrap();
    r.create_index(&IndexSpec {
        name: "todos_by_owner".into(),
        table: "app.todos".into(),
        columns: vec![
            IndexKey::asc("owner"),
            IndexKey {
                nulls_first: false,
                ..IndexKey::desc("done")
            },
        ],
        unique: false,
        nones_distinct: true,
    })
    .await
    .unwrap();
    r.create_index(&IndexSpec {
        name: "users_name".into(),
        table: "users".into(),
        columns: vec![IndexKey::asc("name")],
        unique: true,
        nones_distinct: false,
    })
    .await
    .unwrap();
    for spec in [
        EntitySpec {
            name: "User".into(),
            table: "users".into(),
            key: "id".into(),
            attributes: vec![AttributeSpec::new("name")],
        },
        EntitySpec {
            name: "Group".into(),
            table: "groups".into(),
            key: "id".into(),
            attributes: Vec::new(),
        },
        EntitySpec {
            name: "app.Todo".into(),
            table: "app.todos".into(),
            key: "id".into(),
            attributes: vec![
                AttributeSpec::reference("owner", "owner", "User"),
                AttributeSpec {
                    name: "label".into(),
                    column: "title".into(),
                    references: None,
                },
            ],
        },
    ] {
        r.declare_entity(&spec).await.unwrap();
    }
    r.declare_membership(&MembershipSpec {
        member: "User".into(),
        group: "Group".into(),
        edges: "user_groups".into(),
        edge_columns: ("member".into(), "grp".into()),
        closure: "user_group_closure".into(),
        closure_columns: ("descendant".into(), "ancestor".into(), "paths".into()),
    })
    .await
    .unwrap();
    r.create_foreign_key(
        &ForeignKeySpec::new("todos_owner", "app.todos", ["owner"], "users", ["id"])
            .on_delete(ReferentialAction::Cascade),
    )
    .await
    .unwrap();
    r.set_function_leakproof(
        "lower",
        &[interlaken_ir::DataType::Scalar(
            interlaken_ir::ScalarType::Text,
        )],
        true,
    )
    .await
    .unwrap();

    assert_eq!(
        catalog_shape(&mut by_text).await,
        catalog_shape(&mut by_rust).await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert the printed schema recreates the catalog on a new database, and
/// prints identically there.
async fn schema_text_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut root) = database(dir.path()).await;
    root.execute_script(SCHEMA).await.unwrap();
    let schema = root.schema_text().await.unwrap();
    assert!(
        schema.contains("(create-namespace app :owner (entity User 1))"),
        "{schema}"
    );
    assert!(
        schema.contains("(set-leakproof lower (text) true)"),
        "{schema}"
    );

    let copy_dir = tempfile::tempdir().unwrap();
    let (copy_engine, mut copy) = database(copy_dir.path()).await;
    copy.execute_script(&schema).await.unwrap();
    assert_eq!(copy.schema_text().await.unwrap(), schema);

    // The IR catalogs agree too, column types included.
    let mut reader = session(&copy_engine, admin());
    let todos = reader
        .query("(plan (get app.todos))", Vec::new())
        .await
        .unwrap();
    assert_eq!(
        todos.types.iter().map(print_type).collect::<Vec<_>>(),
        ["int8", "int8", "text", "bool", "(option text)"]
    );
}

/// Return the SQLSTATE of a call that must fail.
fn sqlstate<T>(result: Result<T, EngineError>) -> &'static str {
    result.map(|_| ()).unwrap_err().sqlstate()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert `query` runs DDL with the Rust call's errors, and `prepare`,
/// `explain` and `query_stream` refuse it.
async fn query_runs_ddl() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut root) = database(dir.path()).await;
    let created = root
        .query(
            "(create-table t (column id int8) (primary-key id))",
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(created.columns.is_empty() && created.row_count() == 0);
    root.query(
        "(plan (insert t (values (columns a) (row 1)) (columns id)))",
        Vec::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        sqlstate(
            root.query(
                "(create-table t (column id int8) (primary-key id))",
                Vec::new()
            )
            .await
        ),
        "42P07"
    );
    assert_eq!(
        sqlstate(
            root.query("(create-index i :table t nope)", Vec::new())
                .await
        ),
        "42703"
    );
    assert_eq!(
        sqlstate(root.query("(drop-table t)", vec![Datum::Int8(1)]).await),
        "08P01"
    );
    assert_eq!(
        sqlstate(root.query("(drop-tabel t)", Vec::new()).await),
        "42601"
    );
    assert_eq!(sqlstate(root.prepare("(drop-table t)").await), "0A000");
    assert_eq!(
        sqlstate(root.explain("(drop-table t)", false).await),
        "0A000"
    );
    assert_eq!(
        sqlstate(root.query_stream("(drop-table t)", Vec::new()).await),
        "0A000"
    );

    // Without admin or ownership, as for the Rust call.
    let mut user = session(&engine, EntityUid::long("User", 7));
    assert_eq!(
        sqlstate(user.query("(drop-table t)", Vec::new()).await),
        "42501"
    );

    // DDL and DML share a transaction, and roll back together.
    root.begin(Default::default()).await.unwrap();
    root.query("(add-column t note (option text))", Vec::new())
        .await
        .unwrap();
    root.query(
        "(plan (insert t (values (columns a b) (row 2 \"x\")) (columns id note)))",
        Vec::new(),
    )
    .await
    .unwrap();
    root.rollback().unwrap();
    let rows = root.query("(plan (get t))", Vec::new()).await.unwrap();
    assert_eq!(rows.columns, ["id"]);
    assert_eq!(rows.row_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a script runs DDL and DML in one transaction, reports each
/// statement, and applies nothing if a statement fails.
async fn scripts_are_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut root) = database(dir.path()).await;
    let report = root
        .execute_script(
            "(create-table t (column id int8) (primary-key id))
             (plan (insert t (values (columns a) (row 1) (row 2)) (columns id) (returning id)))
             ; Backfill-style read in the same transaction.
             (plan (get t))",
        )
        .await
        .unwrap();
    assert_eq!(
        report
            .statements
            .iter()
            .map(|s| (s.form, s.line, s.rows))
            .collect::<Vec<_>>(),
        [("create-table", 1, 0), ("plan", 2, 2), ("plan", 4, 2)]
    );
    assert!(report.commit_ts.is_some());

    let error = root
        .execute_script(
            "(create-table u (column id int8) (primary-key id))
             (plan (insert t (values (columns a) (row 1)) (columns id)))",
        )
        .await
        .unwrap_err();
    let EngineError::Script {
        index,
        ref form,
        line,
        ..
    } = error
    else {
        panic!("{error}")
    };
    assert_eq!((index, form.as_str(), line), (2, "plan", 2));
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(
        root.query("(plan (get u))", Vec::new())
            .await
            .unwrap_err()
            .sqlstate(),
        "42P01",
        "rolled back"
    );
    let syntax = root
        .execute_script("(create-table v)\n(oops)")
        .await
        .unwrap_err();
    assert_eq!(syntax.sqlstate(), "42601");

    // In an open transaction, the script's statements are the caller's.
    root.begin(Default::default()).await.unwrap();
    root.execute_script("(create-table w (column id int8) (primary-key id))")
        .await
        .unwrap();
    root.query("(plan (get w))", Vec::new()).await.unwrap();
    root.rollback().unwrap();
    assert_eq!(
        root.query("(plan (get w))", Vec::new())
            .await
            .unwrap_err()
            .sqlstate(),
        "42P01"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert an index, entity type or foreign key named with a namespace must
/// be in its table's namespace.
async fn child_names_follow_their_table() {
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut root) = database(dir.path()).await;
    root.execute_script(
        "(create-namespace app :owner (entity User 1))
         (create-namespace crm :owner (entity User 2))
         (create-table app.t (column id int8) (primary-key id))
         (create-index app.by_id :table app.t id)",
    )
    .await
    .unwrap();
    let schema = root.schema_text().await.unwrap();
    assert!(
        schema.contains("(create-index app.by_id id :table app.t)"),
        "{schema}"
    );
    let error = root
        .query("(create-index crm.by_id :table app.t id)", Vec::new())
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P16", "{error}");
}
