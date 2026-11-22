//! Foreign key declarations (ADR-0035 phase 1): creating and dropping them
//! through the engine, checking existing rows, rejecting declarations that
//! don't fit their tables, and guarding the tables and indexes they use.

mod support;

use interlaken_engine::catalog::{
    AttributeSpec, EntitySpec, ForeignKeySpec, IndexKey, IndexSpec, ReferentialAction, load_catalog,
};
use interlaken_engine::{Engine, EngineError, Session};
use interlaken_ir::catalog::{Catalog, ForeignKeyDef};
use interlaken_ir::security::EntityUid;
use interlaken_storage::{Isolation, Storage, TxnOptions};

use support::session::{admin, create, options, session, table};

/// Return the SQLSTATE and message of a failed result.
fn failure<T: std::fmt::Debug>(result: Result<T, EngineError>) -> (&'static str, String) {
    let err = result.unwrap_err();
    (err.sqlstate(), err.to_string())
}

/// Run each statement, panicking on failure.
async fn run(session: &mut Session, statements: &[&str]) {
    for text in statements {
        session
            .query(text, Vec::new())
            .await
            .unwrap_or_else(|e| panic!("{e}: {text}"));
    }
}

/// Return the foreign keys of the engine's committed catalog.
async fn foreign_keys(engine: &Engine) -> Vec<ForeignKeyDef> {
    let txn = engine.storage().begin(TxnOptions::default()).await.unwrap();
    let catalog = load_catalog(&txn).await.unwrap();
    txn.abort();
    catalog
        .foreign_keys()
        .iter()
        .map(|fk| (**fk).clone())
        .collect()
}

/// Return an index spec on `columns` of `table`.
fn index(name: &str, table: &str, columns: &[&str], unique: bool) -> IndexSpec {
    IndexSpec {
        name: name.into(),
        table: table.into(),
        columns: columns.iter().map(|c| IndexKey::asc(*c)).collect(),
        unique,
        nones_distinct: true,
    }
}

/// Create `users(id, email)` with a unique index on `email`, and
/// `docs(id, owner, editor_email, folder)` with an index on `owner`.
async fn schema(engine: &Engine) -> Session {
    let mut root = session(engine, admin());
    root.create_table(&table(
        "users",
        &[("id", "int8"), ("email", "(option text)")],
        &["id"],
    ))
    .await
    .unwrap();
    root.create_index(&index("users_email", "users", &["email"], true))
        .await
        .unwrap();
    root.create_table(&table(
        "docs",
        &[
            ("id", "int8"),
            ("owner", "(option int8)"),
            ("editor_email", "(option text)"),
            ("parent", "(option int8)"),
        ],
        &["id"],
    ))
    .await
    .unwrap();
    for (name, column) in [
        ("docs_owner", "owner"),
        ("docs_editor", "editor_email"),
        ("docs_parent", "parent"),
    ] {
        root.create_index(&index(name, "docs", &[column], false))
            .await
            .unwrap();
    }
    root
}

#[tokio::test(flavor = "multi_thread")]
/// Assert a foreign key is recorded with its columns, index and options,
/// survives a restart, and can be dropped.
async fn declarations_are_recorded() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = create(dir.path()).await;
        let mut root = schema(&engine).await;
        root.create_foreign_key(
            &ForeignKeySpec::new("docs_owner_fk", "docs", ["owner"], "users", ["id"])
                .on_delete(ReferentialAction::Cascade),
        )
        .await
        .unwrap();
        root.create_foreign_key(
            &ForeignKeySpec::new(
                "docs_editor_fk",
                "docs",
                ["editor_email"],
                "users",
                ["email"],
            )
            .on_delete(ReferentialAction::SetNull)
            .match_full(),
        )
        .await
        .unwrap();
        root.create_foreign_key(&ForeignKeySpec::new(
            "docs_parent_fk",
            "docs",
            ["parent"],
            "docs",
            ["id"],
        ))
        .await
        .unwrap();
    }
    let engine = Engine::open(dir.path(), options()).await.unwrap();
    let fks = foreign_keys(&engine).await;
    let names: Vec<&str> = fks.iter().map(|fk| fk.name.as_str()).collect();
    assert_eq!(names, ["docs_editor_fk", "docs_owner_fk", "docs_parent_fk"]);
    let editor = &fks[0];
    assert_eq!(
        editor.referenced_index.as_ref().map(|i| i.as_str()),
        Some("users_email")
    );
    assert_eq!(editor.on_delete, ReferentialAction::SetNull);
    assert!(editor.match_full);
    let owner = &fks[1];
    assert_eq!(owner.referenced_index, None);
    assert_eq!(owner.on_delete, ReferentialAction::Cascade);
    assert!(!owner.match_full);
    assert_eq!(
        (owner.columns[0].get(), owner.referenced_columns[0].get()),
        (1, 0)
    );
    let txn = engine.storage().begin(TxnOptions::default()).await.unwrap();
    let catalog = load_catalog(&txn).await.unwrap();
    let docs = catalog.table_by_name(&"docs".into()).unwrap().id;
    let users = catalog.table_by_name(&"users".into()).unwrap().id;
    let named = |fks: Vec<std::sync::Arc<ForeignKeyDef>>| -> Vec<String> {
        fks.iter().map(|fk| fk.name.to_string()).collect()
    };
    assert_eq!(named(catalog.foreign_keys_of(users)), Vec::<String>::new());
    assert_eq!(
        named(catalog.foreign_keys_referencing(users)),
        ["docs_editor_fk", "docs_owner_fk"]
    );
    assert_eq!(
        named(catalog.foreign_keys_referencing(docs)),
        ["docs_parent_fk"]
    );
    txn.abort();

    let mut root = session(&engine, admin());
    let before = schema_version(&engine).await;
    root.drop_foreign_key("docs_owner_fk").await.unwrap();
    assert_eq!(foreign_keys(&engine).await.len(), 2);
    assert_eq!(schema_version(&engine).await, before + 1);
    let (state, message) = failure(root.drop_foreign_key("docs_owner_fk").await);
    assert_eq!(state, "42P16", "{message}");
}

/// Return the engine's committed schema version.
async fn schema_version(engine: &Engine) -> u64 {
    let txn = engine.storage().begin(TxnOptions::default()).await.unwrap();
    let version = load_catalog(&txn).await.unwrap().schema_version().get();
    txn.abort();
    version
}

#[tokio::test(flavor = "multi_thread")]
/// Assert creating a foreign key checks existing rows: dangling references
/// fail with 23503 and record nothing; NULLs are skipped under MATCH
/// SIMPLE, and partly NULL keys fail under MATCH FULL.
async fn existing_rows_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = schema(&engine).await;
    run(
        &mut root,
        &[
            "(plan (insert users (values (columns id email) (row 1 \"a@x\") (row 2 null)) \
             (columns id email)))",
            "(plan (insert docs (values (columns id owner editor_email parent) \
             (row 10 1 \"a@x\" null) (row 11 null null 10) (row 12 7 \"b@x\" 99)) \
             (columns id owner editor_email parent)))",
        ],
    )
    .await;
    let owner = ForeignKeySpec::new("docs_owner_fk", "docs", ["owner"], "users", ["id"]);
    let (state, message) = failure(root.create_foreign_key(&owner).await);
    assert_eq!(state, "23503", "{message}");
    assert!(
        message.contains("a row of docs refers to a row of users with id = Int8(7)"),
        "{message}"
    );
    assert!(foreign_keys(&engine).await.is_empty());
    let editor = ForeignKeySpec::new(
        "docs_editor_fk",
        "docs",
        ["editor_email"],
        "users",
        ["email"],
    );
    let (state, message) = failure(root.create_foreign_key(&editor).await);
    assert_eq!(state, "23503", "{message}");
    assert!(message.contains("email = Text(\"b@x\")"), "{message}");

    run(
        &mut root,
        &[
            "(plan (update docs (filter (get docs) (= id 12)) (set owner 2) \
           (set editor_email null) (set parent 11)))",
        ],
    )
    .await;
    // Row 11's NULL owner isn't checked under MATCH SIMPLE.
    root.create_foreign_key(&owner).await.unwrap();
    root.create_foreign_key(&editor).await.unwrap();
    root.create_foreign_key(&ForeignKeySpec::new(
        "docs_parent_fk",
        "docs",
        ["parent"],
        "docs",
        ["id"],
    ))
    .await
    .unwrap();
    assert_eq!(foreign_keys(&engine).await.len(), 3);

    // MATCH FULL: a composite key may be all NULL, but not partly NULL.
    root.create_table(&table(
        "pairs",
        &[("a", "int8"), ("b", "int8")],
        &["a", "b"],
    ))
    .await
    .unwrap();
    root.create_table(&table(
        "refs",
        &[
            ("id", "int8"),
            ("a", "(option int8)"),
            ("b", "(option int8)"),
        ],
        &["id"],
    ))
    .await
    .unwrap();
    root.create_index(&index("refs_ab", "refs", &["b", "a"], false))
        .await
        .unwrap();
    run(
        &mut root,
        &[
            "(plan (insert pairs (values (columns a b) (row 1 2)) (columns a b)))",
            "(plan (insert refs (values (columns id a b) (row 1 1 2) (row 2 null null) \
             (row 3 5 null)) (columns id a b)))",
        ],
    )
    .await;
    let full =
        ForeignKeySpec::new("refs_full", "refs", ["b", "a"], "pairs", ["b", "a"]).match_full();
    let (state, message) = failure(root.create_foreign_key(&full).await);
    assert_eq!(state, "23503", "{message}");
    assert!(message.contains("matches FULL"), "{message}");
    let simple = ForeignKeySpec::new("refs_simple", "refs", ["b", "a"], "pairs", ["b", "a"]);
    root.create_foreign_key(&simple).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
/// Assert declarations that don't fit their tables fail with 42P16 and record nothing.
async fn invalid_declarations_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = schema(&engine).await;
    root.create_table(&table(
        "notes",
        &[("id", "int8"), ("owner", "int4"), ("doc", "int8")],
        &["id"],
    ))
    .await
    .unwrap();
    root.create_index(&index("notes_owner", "notes", &["owner"], false))
        .await
        .unwrap();
    let cases = [
        (
            ForeignKeySpec::new("bad", "notes", ["owner"], "users", ["id"]),
            "notes.owner has type int4, but the column it refers to, users.id, has type int8",
        ),
        (
            ForeignKeySpec::new("bad", "docs", ["editor_email"], "docs", ["editor_email"]),
            "the referenced columns must be the primary key of docs or the columns of one of \
             its unique indexes",
        ),
        (
            ForeignKeySpec::new("bad", "notes", ["doc"], "docs", ["id"]),
            "notes needs an index whose leading columns are doc",
        ),
        (
            ForeignKeySpec::new("bad", "notes", ["id"], "docs", ["id"])
                .on_delete(ReferentialAction::SetNull),
            "on delete set-null needs notes.id to have an optional type",
        ),
        (
            ForeignKeySpec::new("bad", "docs", ["owner", "parent"], "users", ["id"]),
            "needs as many referenced columns as referencing columns",
        ),
        (
            ForeignKeySpec::new("bad", "docs", ["owner"], "engine_entities", ["name"]),
            "reserved",
        ),
    ];
    for (spec, expected) in cases {
        let (state, message) = failure(root.create_foreign_key(&spec).await);
        assert_eq!(state, "42P16", "{message}");
        assert!(message.contains(expected), "{message}");
    }
    let (state, message) = failure(
        root.create_foreign_key(&ForeignKeySpec::new(
            "bad",
            "docs",
            ["missing"],
            "users",
            ["id"],
        ))
        .await,
    );
    assert_eq!(state, "42703", "{message}");
    assert!(foreign_keys(&engine).await.is_empty());

    let owner = ForeignKeySpec::new("docs_owner_fk", "docs", ["owner"], "users", ["id"]);
    root.create_foreign_key(&owner).await.unwrap();
    let (state, message) = failure(root.create_foreign_key(&owner).await);
    assert_eq!(state, "42P16", "{message}");
    assert!(message.contains("already exists"), "{message}");

    let mut user = session(&engine, EntityUid::long("User", 1));
    let (state, _) = failure(user.drop_foreign_key("docs_owner_fk").await);
    assert_eq!(state, "42501");
}

#[tokio::test(flavor = "multi_thread")]
/// Assert a foreign key and an entity attribute on the same column must
/// refer to the same entity type's key, in either declaration order, and
/// that a primary key prefix serves as the referencing index.
async fn references_must_agree() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = schema(&engine).await;
    root.create_table(&table("groups", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    for (name, table) in [("User", "users"), ("Group", "groups")] {
        root.declare_entity(&EntitySpec {
            name: name.into(),
            table: table.into(),
            key: "id".into(),
            attributes: vec![],
        })
        .await
        .unwrap();
    }
    root.declare_entity(&EntitySpec {
        name: "Doc".into(),
        table: "docs".into(),
        key: "id".into(),
        attributes: vec![AttributeSpec::reference("owner", "owner", "Group")],
    })
    .await
    .unwrap();
    let owner = ForeignKeySpec::new("docs_owner_fk", "docs", ["owner"], "users", ["id"]);
    let (state, message) = failure(root.create_foreign_key(&owner).await);
    assert_eq!(state, "42P16", "{message}");
    assert!(
        message.contains("attribute owner of entity type Doc refers to entity type Group"),
        "{message}"
    );
    root.drop_entity("Doc").await.unwrap();
    root.create_foreign_key(&owner).await.unwrap();
    let (state, message) = failure(
        root.declare_entity(&EntitySpec {
            name: "Doc".into(),
            table: "docs".into(),
            key: "id".into(),
            attributes: vec![AttributeSpec::reference("owner", "owner", "Group")],
        })
        .await,
    );
    assert_eq!(state, "42P16", "{message}");
    root.declare_entity(&EntitySpec {
        name: "Doc".into(),
        table: "docs".into(),
        key: "id".into(),
        attributes: vec![AttributeSpec::reference("owner", "owner", "User")],
    })
    .await
    .unwrap();

    // An attribute table keyed by its entity: the primary key leads with
    // the referencing column, so no other index is needed.
    root.create_table(&table(
        "users_phone",
        &[("user_id", "int8"), ("phone", "text")],
        &["user_id"],
    ))
    .await
    .unwrap();
    root.create_foreign_key(&ForeignKeySpec::new(
        "users_phone_user",
        "users_phone",
        ["user_id"],
        "users",
        ["id"],
    ))
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
/// Assert the tables and indexes a foreign key uses can't be dropped from
/// under it, and dropping the referencing table drops its foreign keys.
async fn tables_and_indexes_are_guarded() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = schema(&engine).await;
    for spec in [
        ForeignKeySpec::new("docs_owner_fk", "docs", ["owner"], "users", ["id"]),
        ForeignKeySpec::new(
            "docs_editor_fk",
            "docs",
            ["editor_email"],
            "users",
            ["email"],
        ),
        ForeignKeySpec::new("docs_parent_fk", "docs", ["parent"], "docs", ["id"]),
    ] {
        root.create_foreign_key(&spec).await.unwrap();
    }
    let (state, message) = failure(root.drop_table("users").await);
    assert_eq!(state, "42P16", "{message}");
    assert!(message.contains("referred to by foreign key"), "{message}");
    let (state, message) = failure(root.drop_index("users_email").await);
    assert_eq!(state, "42P16", "{message}");
    assert!(message.contains("docs_editor_fk"), "{message}");
    let (state, message) = failure(root.drop_index("docs_owner").await);
    assert_eq!(state, "42P16", "{message}");
    assert!(
        message.contains("docs needs an index whose leading columns are owner"),
        "{message}"
    );
    // Another index leading with the column takes over.
    root.create_index(&index("docs_owner_id", "docs", &["owner", "id"], false))
        .await
        .unwrap();
    root.drop_index("docs_owner").await.unwrap();
    assert_eq!(foreign_keys(&engine).await.len(), 3);

    // Dropping docs drops its own foreign keys, the self-reference included.
    root.drop_table("docs").await.unwrap();
    assert!(foreign_keys(&engine).await.is_empty());
    root.drop_table("users").await.unwrap();
}

/// Return the rows of `text`.
async fn rows(session: &mut Session, text: &str) -> Vec<Vec<interlaken_ir::interp::Datum>> {
    session
        .query(text, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}: {text}"))
        .rows()
}

/// Return the SQLSTATE and message of running `text`, which must fail.
async fn fails(session: &mut Session, text: &str) -> (&'static str, String) {
    failure(session.query(text, Vec::new()).await)
}

/// Create [`schema`] with users 1 and 2, the three foreign keys of
/// `declarations_are_recorded` (all `NoAction`), and documents 10 (owner
/// 1) and 11 (child of 10, owner 2).
async fn populated(engine: &Engine) -> Session {
    let mut root = schema(engine).await;
    run(
        &mut root,
        &[
            "(plan (insert users (values (columns id email) (row 1 \"a@x\") (row 2 \"b@x\")) \
             (columns id email)))",
            "(plan (insert docs (values (columns id owner editor_email parent) \
             (row 10 1 \"a@x\" null) (row 11 2 null 10)) (columns id owner editor_email parent)))",
        ],
    )
    .await;
    for spec in [
        ForeignKeySpec::new("docs_owner_fk", "docs", ["owner"], "users", ["id"]),
        ForeignKeySpec::new(
            "docs_editor_fk",
            "docs",
            ["editor_email"],
            "users",
            ["email"],
        ),
        ForeignKeySpec::new("docs_parent_fk", "docs", ["parent"], "docs", ["id"]),
    ] {
        root.create_foreign_key(&spec).await.unwrap();
    }
    root
}

/// Return the ids of every document, in order.
async fn doc_ids(session: &mut Session) -> Vec<i64> {
    rows(session, "(plan (sort (project (get docs) id) (asc id)))")
        .await
        .into_iter()
        .map(|r| match r[0] {
            interlaken_ir::interp::Datum::Int8(v) => v,
            ref other => panic!("{other:?}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
/// Assert inserts, updates and deletes are checked against foreign keys in
/// both directions, through primary keys and unique indexes, and a
/// violating statement fails with 23503 and leaves nothing behind.
async fn writes_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = populated(&engine).await;
    let cases = [
        (
            "(plan (insert docs (values (columns id owner) (row 20 1) (row 21 7)) (columns id owner)))",
            "foreign key docs_owner_fk: a row of docs refers to a row of users with id = Int8(7), \
             which doesn't exist",
        ),
        (
            "(plan (insert docs (values (columns id editor_email) (row 20 \"c@x\")) \
             (columns id editor_email)))",
            "foreign key docs_editor_fk: a row of docs refers to a row of users with email = \
             Text(\"c@x\")",
        ),
        (
            "(plan (update docs (filter (get docs) (= id 11)) (set owner 9)))",
            "a row of docs refers to a row of users with id = Int8(9)",
        ),
        (
            "(plan (delete users (filter (get users) (= id 2))))",
            "foreign key docs_owner_fk: rows of docs still refer to the row of users with id = \
             Int8(2)",
        ),
        (
            "(plan (update users (filter (get users) (= id 1)) (set email \"z@x\")))",
            "foreign key docs_editor_fk: rows of docs still refer to the row of users with email \
             = Text(\"a@x\")",
        ),
        (
            "(plan (delete docs (filter (get docs) (= id 10))))",
            "foreign key docs_parent_fk: rows of docs still refer to the row of docs with id = \
             Int8(10)",
        ),
    ];
    for (text, expected) in cases {
        let (state, message) = fails(&mut root, text).await;
        assert_eq!(state, "23503", "{text}: {message}");
        assert!(message.contains(expected), "{text}: {message}");
    }
    // Nothing was written: the first insert's valid row 20 is gone too.
    assert_eq!(doc_ids(&mut root).await, [10, 11]);

    // Statements that keep every reference valid succeed.
    run(
        &mut root,
        &[
            "(plan (insert docs (values (columns id owner editor_email parent) \
             (row 20 null null null) (row 21 2 \"b@x\" 11)) (columns id owner editor_email parent)))",
            "(plan (update users (filter (get users) (= id 2)) (set email \"b@x\")))",
            "(plan (update users (filter (get users) (= id 1)) (set email (concat email \"\"))))",
            "(plan (update docs (filter (get docs) (= id 21)) (set owner 1) (set editor_email null)))",
            "(plan (delete docs (filter (get docs) (= id 21))))",
            "(plan (update users (filter (get users) (= id 2)) (set email \"c@x\")))",
        ],
    )
    .await;
    // Inside a transaction, the failed statement's writes are undone
    // before the transaction is rolled back.
    root.begin(TxnOptions::default()).await.unwrap();
    run(
        &mut root,
        &["(plan (delete docs (filter (get docs) (= id 20))))"],
    )
    .await;
    let (state, _) = fails(
        &mut root,
        "(plan (delete users (filter (get users) (= id 1))))",
    )
    .await;
    assert_eq!(state, "23503");
    root.rollback().unwrap();
    assert_eq!(doc_ids(&mut root).await, [10, 11, 20]);
}

#[tokio::test(flavor = "multi_thread")]
/// Assert foreign keys must hold when each statement ends, not after each
/// row: one statement may insert a self-referencing tree children first,
/// or delete a whole subtree.
async fn checks_hold_at_statement_end() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = populated(&engine).await;
    // Children before their parents, spread over several batches.
    let rows_text: String = (0..3000)
        .rev()
        .map(|i| {
            let parent = if i == 0 {
                "null".to_owned()
            } else {
                (100 + (i - 1) / 2).to_string()
            };
            format!(" (row {} 1 {parent})", 100 + i)
        })
        .collect();
    run(
        &mut root,
        &[&format!(
            "(plan (insert docs (values (columns id owner parent){rows_text}) \
             (columns id owner parent)))"
        )],
    )
    .await;
    assert_eq!(doc_ids(&mut root).await.len(), 3002);
    // Deleting the tree's root alone fails; deleting the whole tree works.
    let (state, _) = fails(
        &mut root,
        "(plan (delete docs (filter (get docs) (= id 100))))",
    )
    .await;
    assert_eq!(state, "23503");
    run(
        &mut root,
        &["(plan (delete docs (filter (get docs) (>= id 100))))"],
    )
    .await;
    assert_eq!(doc_ids(&mut root).await, [10, 11]);
    // A row may refer to itself.
    run(
        &mut root,
        &[
            "(plan (insert docs (values (columns id owner parent) (row 30 1 30)) \
           (columns id owner parent)))",
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
/// Assert MATCH FULL rejects partly NULL keys on write, and that a
/// cascade through a composite key deletes exactly the rows referring to
/// the deleted key.
async fn match_full_and_composite_cascades() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = schema(&engine).await;
    root.create_table(&table(
        "pairs",
        &[("a", "int8"), ("b", "int8")],
        &["a", "b"],
    ))
    .await
    .unwrap();
    root.create_table(&table(
        "refs",
        &[
            ("id", "int8"),
            ("a", "(option int8)"),
            ("b", "(option int8)"),
        ],
        &["id"],
    ))
    .await
    .unwrap();
    root.create_index(&index("refs_ab", "refs", &["a", "b"], false))
        .await
        .unwrap();
    root.create_foreign_key(
        &ForeignKeySpec::new("refs_fk", "refs", ["a", "b"], "pairs", ["a", "b"])
            .match_full()
            .on_delete(ReferentialAction::Cascade),
    )
    .await
    .unwrap();
    run(
        &mut root,
        &[
            "(plan (insert pairs (values (columns a b) (row 1 2) (row 1 3)) (columns a b)))",
            "(plan (insert refs (values (columns id a b) (row 1 1 2) (row 2 null null) \
             (row 3 1 3)) (columns id a b)))",
        ],
    )
    .await;
    let (state, message) = fails(
        &mut root,
        "(plan (insert refs (values (columns id a b) (row 4 1 (cast null int8))) (columns id a b)))",
    )
    .await;
    assert_eq!(state, "23503", "{message}");
    assert!(message.contains("matches FULL"), "{message}");
    run(
        &mut root,
        &["(plan (delete pairs (filter (get pairs) (= b 2))))"],
    )
    .await;
    let ids = rows(&mut root, "(plan (sort (project (get refs) id) (asc id)))").await;
    assert_eq!(
        ids,
        [2, 3].map(|i| vec![interlaken_ir::interp::Datum::Int8(i)])
    );
}

#[tokio::test(flavor = "multi_thread")]
/// Assert EXPLAIN names the foreign keys a write checks.
async fn explain_shows_checked_foreign_keys() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = populated(&engine).await;
    let plan = root
        .explain("(plan (delete users (filter (get users) (= id 9))))", false)
        .await
        .unwrap();
    assert!(
        plan.contains("checking foreign keys: docs_editor_fk, docs_owner_fk"),
        "{plan}"
    );
    let plan = root
        .explain(
            "(plan (insert docs (values (columns id) (row 50)) (columns id)))",
            false,
        )
        .await
        .unwrap();
    assert!(
        plan.contains("checking foreign keys: docs_editor_fk, docs_owner_fk, docs_parent_fk"),
        "{plan}"
    );
}

#[tokio::test(flavor = "multi_thread")]
/// Assert a concurrent "insert a child" and "delete its parent" can't both
/// commit, in either commit order, at every isolation level.
async fn concurrent_writes_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = populated(&engine).await;
    let mut other = session(&engine, admin());
    let mut id = 1000;
    for isolation in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        for child_commits_first in [false, true] {
            id += 1;
            run(
                &mut root,
                &[&format!(
                    "(plan (insert users (values (columns id) (row {id})) (columns id)))"
                )],
            )
            .await;
            root.begin(TxnOptions::new(isolation)).await.unwrap();
            other.begin(TxnOptions::new(isolation)).await.unwrap();
            run(
                &mut root,
                &[&format!(
                    "(plan (insert docs (values (columns id owner) (row {id} {id})) \
                     (columns id owner)))"
                )],
            )
            .await;
            run(
                &mut other,
                &[&format!(
                    "(plan (delete users (filter (get users) (= id {id}))))"
                )],
            )
            .await;
            let (first, second) = if child_commits_first {
                (&mut root, &mut other)
            } else {
                (&mut other, &mut root)
            };
            first.commit().await.unwrap();
            let (state, message) = failure(second.commit().await);
            assert_eq!(
                state, "40001",
                "{isolation:?}, child first: {child_commits_first}: {message}"
            );
        }
    }
    // No document refers to a missing user.
    let dangling = rows(
        &mut root,
        "(plan (join anti (get docs :as d) (get users :as u) (= d.owner u.id)))",
    )
    .await;
    assert!(
        dangling
            .iter()
            .all(|r| r[1] == interlaken_ir::interp::Datum::Null),
        "{dangling:?}"
    );
}

/// Return the rows of `text` as `Debug` strings, for compact comparisons.
async fn shown(session: &mut Session, text: &str) -> Vec<String> {
    rows(session, text)
        .await
        .iter()
        .map(|r| format!("{r:?}"))
        .collect()
}

/// Create `users(id)`, `docs(id, owner, parent)` and `comments(id, doc)`,
/// with docs' owner and parent cascading and comments' doc set to NULL on delete.
async fn cascading_schema(engine: &Engine) -> Session {
    let mut root = session(engine, admin());
    root.create_table(&table("users", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    root.create_table(&table(
        "docs",
        &[
            ("id", "int8"),
            ("owner", "int8"),
            ("parent", "(option int8)"),
        ],
        &["id"],
    ))
    .await
    .unwrap();
    root.create_table(&table(
        "comments",
        &[("id", "int8"), ("doc", "(option int8)")],
        &["id"],
    ))
    .await
    .unwrap();
    for (name, table, column) in [
        ("docs_owner", "docs", "owner"),
        ("docs_parent", "docs", "parent"),
        ("comments_doc", "comments", "doc"),
    ] {
        root.create_index(&index(name, table, &[column], false))
            .await
            .unwrap();
    }
    for spec in [
        ForeignKeySpec::new("docs_owner_fk", "docs", ["owner"], "users", ["id"])
            .on_delete(ReferentialAction::Cascade),
        ForeignKeySpec::new("docs_parent_fk", "docs", ["parent"], "docs", ["id"])
            .on_delete(ReferentialAction::Cascade),
        ForeignKeySpec::new("comments_doc_fk", "comments", ["doc"], "docs", ["id"])
            .on_delete(ReferentialAction::SetNull),
    ] {
        root.create_foreign_key(&spec).await.unwrap();
    }
    run(
        &mut root,
        &[
            "(plan (insert users (values (columns id) (row 1) (row 2)) (columns id)))",
            "(plan (insert docs (values (columns id owner parent) (row 10 1 null) (row 11 2 10) \
             (row 12 2 null)) (columns id owner parent)))",
            "(plan (insert comments (values (columns id doc) (row 100 11) (row 101 12)) \
             (columns id doc)))",
        ],
    )
    .await;
    root
}

#[tokio::test(flavor = "multi_thread")]
/// Assert deletes cascade transitively through foreign keys: deleting a
/// user deletes their documents and those documents' children (owned by
/// anyone), and sets comments on deleted documents to NULL.
async fn deletes_cascade() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = cascading_schema(&engine).await;
    let plan = root
        .explain("(plan (delete docs (filter (get docs) (= id 99))))", false)
        .await
        .unwrap();
    assert!(
        plan.contains("cascading to: comments_doc_fk (set-null), docs_parent_fk (cascade)"),
        "{plan}"
    );
    run(
        &mut root,
        &["(plan (delete users (filter (get users) (= id 1))))"],
    )
    .await;
    assert_eq!(doc_ids(&mut root).await, [12]);
    assert_eq!(
        shown(&mut root, "(plan (sort (get comments) (asc id)))").await,
        ["[Int8(100), Null]", "[Int8(101), Int8(12)]"]
    );
    // A deep self-referencing tree cascades without recursing per level.
    let chain: String = (0..2000)
        .map(|i| {
            let parent = if i == 0 {
                "null".to_owned()
            } else {
                (1000 + i - 1).to_string()
            };
            format!(" (row {} 2 {parent})", 1000 + i)
        })
        .collect();
    run(
        &mut root,
        &[&format!(
            "(plan (insert docs (values (columns id owner parent){chain}) (columns id owner parent)))"
        )],
    )
    .await;
    assert_eq!(doc_ids(&mut root).await.len(), 2001);
    run(
        &mut root,
        &["(plan (delete docs (filter (get docs) (= id 1000))))"],
    )
    .await;
    assert_eq!(doc_ids(&mut root).await, [12]);
}

#[tokio::test(flavor = "multi_thread")]
/// Assert a cascade that removes a row a `NO ACTION` foreign key still
/// refers to fails the whole statement with 23503, changing nothing.
async fn cascades_meet_no_action() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = cascading_schema(&engine).await;
    root.create_table(&table("notes", &[("id", "int8"), ("doc", "int8")], &["id"]))
        .await
        .unwrap();
    root.create_index(&index("notes_doc", "notes", &["doc"], false))
        .await
        .unwrap();
    root.create_foreign_key(&ForeignKeySpec::new(
        "notes_doc_fk",
        "notes",
        ["doc"],
        "docs",
        ["id"],
    ))
    .await
    .unwrap();
    run(
        &mut root,
        &["(plan (insert notes (values (columns id doc) (row 1 11)) (columns id doc)))"],
    )
    .await;
    let (state, message) = fails(
        &mut root,
        "(plan (delete users (filter (get users) (= id 1))))",
    )
    .await;
    assert_eq!(state, "23503", "{message}");
    assert!(message.contains("notes_doc_fk"), "{message}");
    assert_eq!(doc_ids(&mut root).await, [10, 11, 12]);
    assert_eq!(
        shown(&mut root, "(plan (sort (get comments) (asc id)))").await,
        ["[Int8(100), Int8(11)]", "[Int8(101), Int8(12)]"]
    );
}

#[tokio::test(flavor = "multi_thread")]
/// Assert a cascade changes every referring row or fails: a principal
/// whose policies let them delete their user row, but not a document
/// someone else owns that refers to it, can't delete the user (42501).
async fn cascades_obey_write_policies() {
    use interlaken_engine::catalog::AttributeSpec;
    use interlaken_ir::interp::Datum;
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    root.create_table(&table("users", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    root.create_table(&table(
        "docs",
        &[
            ("id", "int8"),
            ("owner_id", "int8"),
            ("editor_id", "(option int8)"),
        ],
        &["id"],
    ))
    .await
    .unwrap();
    root.create_index(&index("docs_editor", "docs", &["editor_id"], false))
        .await
        .unwrap();
    root.create_foreign_key(
        &ForeignKeySpec::new("docs_editor_fk", "docs", ["editor_id"], "users", ["id"])
            .on_delete(ReferentialAction::Cascade),
    )
    .await
    .unwrap();
    root.declare_entity(&EntitySpec {
        name: "User".into(),
        table: "users".into(),
        key: "id".into(),
        attributes: vec![],
    })
    .await
    .unwrap();
    root.declare_entity(&EntitySpec {
        name: "Doc".into(),
        table: "docs".into(),
        key: "id".into(),
        attributes: vec![AttributeSpec::reference("owner", "owner_id", "User")],
    })
    .await
    .unwrap();
    let users = vec![vec![Datum::Int8(1)], vec![Datum::Int8(2)]];
    let docs = |editor_of_11: i64| {
        vec![
            vec![Datum::Int8(10), Datum::Int8(1), Datum::Int8(1)],
            vec![Datum::Int8(11), Datum::Int8(2), Datum::Int8(editor_of_11)],
        ]
    };
    support::session::replace_rows(&engine, &[("users", users.clone()), ("docs", docs(1))]).await;
    support::session::activate_rls(
        &mut root,
        "(policies
           (permit (read delete) User true)
           (permit (read delete) Doc (== resource.owner principal)))",
    )
    .await
    .unwrap();
    let mut alice = session(&engine, EntityUid::long("User", 1));
    let delete_self = "(plan (delete users (filter (get users) (= id 1))))";
    let (state, message) = fails(&mut alice, delete_self).await;
    assert_eq!(state, "42501", "{message}");
    assert!(message.contains("docs"), "{message}");
    assert_eq!(
        shown(&mut alice, "(plan (project (get users) id))").await,
        ["[Int8(1)]", "[Int8(2)]"]
    );
    // With doc 11 edited by someone else, only alice's own doc cascades.
    support::session::replace_rows(&engine, &[("users", users), ("docs", docs(2))]).await;
    run(&mut alice, &[delete_self]).await;
    assert_eq!(
        shown(&mut alice, "(plan (project (get users) id))").await,
        ["[Int8(2)]"]
    );
    let mut bob = session(&engine, EntityUid::long("User", 2));
    assert_eq!(
        shown(&mut bob, "(plan (project (get docs) id))").await,
        ["[Int8(11)]"]
    );
}

#[tokio::test(flavor = "multi_thread")]
/// Assert deleting an entity cascades to its membership edges, and the
/// closure loses the pairs those edges made, as for any edge delete.
async fn cascades_maintain_closures() {
    use interlaken_engine::catalog::MembershipSpec;
    let dir = tempfile::tempdir().unwrap();
    let engine = create(dir.path()).await;
    let mut root = session(&engine, admin());
    for (name, columns, key) in [
        ("users", vec![("id", "int8")], vec!["id"]),
        ("groups", vec![("id", "int8")], vec!["id"]),
        (
            "edges",
            vec![("member", "int8"), ("grp", "int8")],
            vec!["member", "grp"],
        ),
        (
            "closure",
            vec![
                ("descendant", "int8"),
                ("ancestor", "int8"),
                ("paths", "int8"),
            ],
            vec!["descendant", "ancestor"],
        ),
    ] {
        root.create_table(&table(name, &columns, &key))
            .await
            .unwrap();
    }
    for (name, table_name) in [("User", "users"), ("Group", "groups")] {
        root.declare_entity(&EntitySpec {
            name: name.into(),
            table: table_name.into(),
            key: "id".into(),
            attributes: vec![],
        })
        .await
        .unwrap();
    }
    root.declare_membership(&MembershipSpec {
        member: "User".into(),
        group: "Group".into(),
        edges: "edges".into(),
        edge_columns: ("member".into(), "grp".into()),
        closure: "closure".into(),
        closure_columns: ("descendant".into(), "ancestor".into(), "paths".into()),
    })
    .await
    .unwrap();
    root.create_foreign_key(
        &ForeignKeySpec::new("edges_member_fk", "edges", ["member"], "users", ["id"])
            .on_delete(ReferentialAction::Cascade),
    )
    .await
    .unwrap();
    support::session::activate_rls(
        &mut root,
        "(policies
           (permit (read insert update delete) User (is principal Admin))
           (permit (read insert update delete) Group (is principal Admin)))",
    )
    .await
    .unwrap();
    run(
        &mut root,
        &[
            "(plan (insert users (values (columns id) (row 1) (row 2)) (columns id)))",
            "(plan (insert groups (values (columns id) (row 7)) (columns id)))",
            "(plan (insert edges (values (columns member grp) (row 1 7) (row 2 7)) \
             (columns member grp)))",
        ],
    )
    .await;
    let closure = "(plan (sort (get closure) (asc descendant)))";
    assert_eq!(
        shown(&mut root, closure).await,
        ["[Int8(1), Int8(7), Int8(1)]", "[Int8(2), Int8(7), Int8(1)]"]
    );
    run(
        &mut root,
        &["(plan (delete users (filter (get users) (= id 1))))"],
    )
    .await;
    assert_eq!(
        shown(&mut root, closure).await,
        ["[Int8(2), Int8(7), Int8(1)]"]
    );
    assert_eq!(
        shown(&mut root, "(plan (get edges))").await,
        ["[Int8(2), Int8(7)]"]
    );
}
