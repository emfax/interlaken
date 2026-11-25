//! Policy sets per namespace, combined with the baseline (ADR-0042 phase 4).

mod support;

use interlaken_engine::catalog::{AttributeSpec, EntitySpec};
use interlaken_engine::policy::{PolicyDocument, PolicyLanguage};
use interlaken_engine::{Engine, EngineError, Session};
use interlaken_ir::security::EntityUid;
use support::session::{admin, create, session, table};

/// Return user `n`.
fn user(n: i64) -> EntityUid {
    EntityUid::long("User", n)
}

/// Return Cedar policies as a document.
fn cedar(body: &str) -> PolicyDocument {
    PolicyDocument {
        language: PolicyLanguage::Cedar,
        body: body.into(),
    }
}

/// Activate `body` as the baseline.
async fn baseline(root: &mut Session, body: &str) {
    let id = root.submit_policies(&cedar(body), "").await.unwrap();
    root.verify_policies(id).await.unwrap();
    root.activate_policies(id, true).await.unwrap();
}

/// Propose, verify and activate `body` as `scope`'s set as `who`.
async fn scoped(who: &mut Session, scope: &str, body: &str) -> Result<i64, EngineError> {
    let id = who.submit_scoped_policies(scope, &cedar(body), "").await?;
    who.verify_policies(id).await?;
    who.activate_policies(id, true).await
}

/// Users 1 to 3 in `public`; namespace `app` (owned by user 1) with
/// `app.Todo` over `app.todos`, and `crm` (owned by user 2) with
/// `crm.Contact` over `crm.contacts`; every user owns one of each.
async fn database(dir: &std::path::Path) -> Engine {
    let engine = create(dir).await;
    let mut root = session(&engine, admin());
    root.create_namespace("app", &user(1)).await.unwrap();
    root.create_namespace("crm", &user(2)).await.unwrap();
    root.create_table(&table("users", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    let owned = |name: &str| table(name, &[("id", "int8"), ("owner", "int8")], &["id"]);
    root.create_table(&owned("app.todos")).await.unwrap();
    root.create_table(&owned("crm.contacts")).await.unwrap();
    for statement in [
        "(plan (insert users (values (columns a) (row 1) (row 2) (row 3)) (columns id)))",
        "(plan (insert app.todos (values (columns a b) (row 1 1) (row 2 2) (row 3 3)) (columns id owner)))",
        "(plan (insert crm.contacts (values (columns a b) (row 1 1) (row 2 2) (row 3 3)) (columns id owner)))",
    ] {
        root.query(statement, Vec::new()).await.unwrap();
    }
    root.declare_entity(&EntitySpec {
        name: "User".into(),
        table: "users".into(),
        key: "id".into(),
        attributes: Vec::new(),
    })
    .await
    .unwrap();
    for (name, table) in [("Todo", "app.todos"), ("Contact", "crm.contacts")] {
        root.declare_entity(&EntitySpec {
            name: name.into(),
            table: table.into(),
            key: "id".into(),
            attributes: vec![AttributeSpec::reference("owner", "owner", "User")],
        })
        .await
        .unwrap();
    }
    baseline(
        &mut root,
        r#"permit (principal is Admin, action, resource);
permit (principal, action == Action::"read", resource is User);
forbid (principal == User::"3", action, resource);"#,
    )
    .await;
    engine
}

/// Return how many rows of `table` `who` reads.
async fn count(engine: &Engine, who: EntityUid, table: &str) -> usize {
    session(engine, who)
        .query(&format!("(plan (get {table}))"), Vec::new())
        .await
        .unwrap()
        .row_count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a namespace's owner governs its tables with its own set, which
/// the baseline's forbids still override, and which reaches no other
/// namespace.
async fn namespaces_govern_their_tables() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    // Nothing grants users anything in either namespace yet.
    assert_eq!(count(&engine, user(1), "app.todos").await, 0);

    let mut owner = session(&engine, user(1));
    scoped(
        &mut owner,
        "app",
        r#"permit (principal, action == Action::"read", resource is app::Todo) when { resource.owner == principal };"#,
    )
    .await
    .unwrap();
    assert_eq!(count(&engine, user(1), "app.todos").await, 1);
    assert_eq!(count(&engine, user(2), "app.todos").await, 1);
    assert_eq!(
        count(&engine, user(3), "app.todos").await,
        0,
        "the baseline forbids user 3"
    );
    assert_eq!(
        count(&engine, user(1), "crm.contacts").await,
        0,
        "app's set reaches only app"
    );
    assert_eq!(
        count(&engine, admin(), "app.todos").await,
        3,
        "the baseline still applies"
    );

    // The owner of app can't touch crm's set.
    let denied = owner
        .submit_scoped_policies("crm", &cedar(""), "")
        .await
        .unwrap_err();
    assert_eq!(denied.sqlstate(), "42501");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a namespace's set can't concern another namespace's types, or
/// leave its resource unconstrained.
async fn namespace_sets_are_confined() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut owner = session(&engine, user(1));
    for body in [
        r#"permit (principal, action == Action::"read", resource is crm::Contact);"#,
        r#"permit (principal, action == Action::"read", resource);"#,
        r#"permit (principal, action == Action::"read", resource is User);"#,
    ] {
        let error = scoped(&mut owner, "app", body).await.unwrap_err();
        assert!(
            matches!(error, EngineError::GateFailed { .. }),
            "{body}: {error}"
        );
    }
    assert_eq!(count(&engine, user(2), "crm.contacts").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert proposals go stale only against their own set: activating app's
/// set leaves crm's proposal current, and the baseline's too.
async fn staleness_is_per_set() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let mut crm_owner = session(&engine, user(2));
    let crm = crm_owner
        .submit_scoped_policies(
            "crm",
            &cedar(r#"permit (principal, action == Action::"read", resource is crm::Contact);"#),
            "",
        )
        .await
        .unwrap();
    crm_owner.verify_policies(crm).await.unwrap();
    let mut root = session(&engine, admin());
    let base = root
        .submit_policies(
            &cedar(
                r#"permit (principal is Admin, action, resource);
permit (principal, action == Action::"read", resource is User);"#,
            ),
            "",
        )
        .await
        .unwrap();
    root.verify_policies(base).await.unwrap();

    scoped(
        &mut session(&engine, user(1)),
        "app",
        r#"permit (principal, action == Action::"read", resource is app::Todo);"#,
    )
    .await
    .unwrap();
    crm_owner.activate_policies(crm, true).await.unwrap();
    root.activate_policies(base, true).await.unwrap();
    // The new baseline no longer forbids user 3; crm's set lets everyone read.
    assert_eq!(count(&engine, user(3), "crm.contacts").await, 3);
    assert_eq!(count(&engine, user(3), "app.todos").await, 3);

    // Dropping a namespace drops its set: a new crm starts with none.
    root.drop_namespace("crm", true).await.unwrap();
    root.create_namespace("crm", &user(2)).await.unwrap();
    root.create_table(&table(
        "crm.contacts",
        &[("id", "int8"), ("owner", "int8")],
        &["id"],
    ))
    .await
    .unwrap();
    root.declare_entity(&EntitySpec {
        name: "Contact".into(),
        table: "crm.contacts".into(),
        key: "id".into(),
        attributes: vec![AttributeSpec::reference("owner", "owner", "User")],
    })
    .await
    .unwrap();
    root.query(
        "(plan (insert crm.contacts (values (columns a b) (row 1 1)) (columns id owner)))",
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(count(&engine, user(3), "crm.contacts").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert RLS policies name namespaced types as `app.Todo`, both in a
/// namespace's set and in the baseline.
async fn rls_names_namespaced_types() {
    let dir = tempfile::tempdir().unwrap();
    let engine = database(dir.path()).await;
    let rls = |body: &str| PolicyDocument {
        language: PolicyLanguage::Rls,
        body: body.into(),
    };
    let mut root = session(&engine, admin());
    let id = root
        .submit_policies(
            &rls("(policies (permit (read insert update delete) app.Todo (is principal Admin)) (forbid (read) app.Todo (== resource.owner (entity User 2))))"),
            "",
        )
        .await
        .unwrap();
    root.verify_policies(id).await.unwrap();
    root.activate_policies(id, true).await.unwrap();
    let id = root
        .submit_scoped_policies(
            "app",
            &rls("(policies (permit (read) app.Todo (== resource.owner principal)))"),
            "",
        )
        .await
        .unwrap();
    root.verify_policies(id).await.unwrap();
    root.activate_policies(id, true).await.unwrap();
    assert_eq!(count(&engine, user(1), "app.todos").await, 1);
    assert_eq!(
        count(&engine, user(2), "app.todos").await,
        0,
        "the baseline forbids it"
    );
    assert_eq!(count(&engine, admin(), "app.todos").await, 2);
}
