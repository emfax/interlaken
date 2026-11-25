//! Principal types with no table (ADR-0042 phase 1, backlog
//! `undeclared-principal-types`).

mod support;

use interlaken_engine::catalog::{AttributeSpec, EntitySpec};
use interlaken_engine::policy::{PolicyDocument, PolicyLanguage};
use interlaken_engine::{Engine, Session};
use interlaken_ir::security::EntityUid;
use support::session::{admin, create, session, table};

/// Activate Cedar `policies` as `root`.
async fn activate_cedar(root: &mut Session, policies: &str) {
    let document = PolicyDocument {
        language: PolicyLanguage::Cedar,
        body: policies.into(),
    };
    let id = root.submit_policies(&document, "").await.unwrap();
    root.verify_policies(id).await.unwrap();
    root.activate_policies(id, true).await.unwrap();
}

/// Create a governed `docs` table, with the `User` type declared too.
async fn database(dir: &std::path::Path) -> Engine {
    let engine = create(dir).await;
    let mut root = session(&engine, admin());
    root.create_table(&table("users", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    root.create_table(&table(
        "docs",
        &[("id", "int8"), ("owner", "int8")],
        &["id"],
    ))
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
        attributes: vec![AttributeSpec::reference("owner", "owner", "User")],
    })
    .await
    .unwrap();
    engine
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert Cedar policies can name the first administrator's type, declared
/// at creation, and declared service types; undeclared types are denied,
/// not invalid.
async fn principal_types_without_tables() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut root = session(&engine, admin());
    activate_cedar(
        &mut root,
        r#"permit (principal is Admin, action, resource);
permit (principal is User, action == Action::"read", resource is Doc) when { resource.owner == principal };"#,
    )
    .await;
    root.query(
        "(plan (insert docs (values (columns a b) (row 1 1) (row 2 2)) (columns id owner)))",
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        root.query("(plan (get docs))", Vec::new())
            .await
            .unwrap()
            .row_count(),
        2
    );

    // An undeclared type sees nothing, without an error.
    let mut service = session(&engine, EntityUid::str("Service", "indexer"));
    assert_eq!(
        service
            .query("(plan (get docs))", Vec::new())
            .await
            .unwrap()
            .row_count(),
        0
    );

    // Declared, and granted by a policy, it reads everything.
    root.declare_principal_type("Service").await.unwrap();
    activate_cedar(
        &mut root,
        r#"permit (principal is Admin, action, resource);
permit (principal is Service, action == Action::"read", resource);"#,
    )
    .await;
    assert_eq!(
        service
            .query("(plan (get docs))", Vec::new())
            .await
            .unwrap()
            .row_count(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a principal type can't take an entity type's name, and an entity
/// type replaces a principal type of the same name.
async fn names_are_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut root = session(&engine, admin());
    assert_eq!(
        root.declare_principal_type("Doc")
            .await
            .unwrap_err()
            .sqlstate(),
        "42P16"
    );
    assert_eq!(
        root.declare_principal_type("Admin")
            .await
            .unwrap_err()
            .sqlstate(),
        "42P16",
        "declared at creation"
    );
    root.create_table(&table("admins", &[("id", "text")], &["id"]))
        .await
        .unwrap();
    let admins = EntitySpec {
        name: "Admin".into(),
        table: "admins".into(),
        key: "id".into(),
        attributes: Vec::new(),
    };
    root.declare_entity(&admins).await.unwrap();
    assert_eq!(
        root.drop_principal_type("Admin")
            .await
            .unwrap_err()
            .sqlstate(),
        "42P16"
    );
}
