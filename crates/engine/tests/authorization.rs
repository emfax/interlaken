//! Transactional authorization (ADR-0019 phase 1, ADR-0021 phase 4, milestone I4).
//!
//! The principal's memberships and the policy set are read through the
//! transaction. A role revoked by a concurrent transaction therefore makes
//! a SERIALIZABLE transaction that relied on it fail at commit, and a
//! concurrent policy change makes a writing transaction fail at every
//! isolation level.

use interlaken_engine::{
    EngineError, create_policy_table, load_policies, load_principal, store_policies,
};
use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::security::{EntityUid, SecValue};
use interlaken_ir::text::parse_catalog;
use interlaken_storage::catalog::{self, ColumnDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::{
    Durability, Error, FjallStorage, Isolation, Storage, StorageOptions, TxnOptions, rows,
};

/// The IR catalog matching the stored tables.
fn ir_catalog() -> InMemoryCatalog {
    parse_catalog(
        "(catalog
           (table users :id 100 (column id (not-null int8)) (column dept text) (primary-key id))
           (table groups :id 101 (column id (not-null int8)) (primary-key id))
           (table memberships :id 102 (column member (not-null int8)) (column grp (not-null int8)) (primary-key member grp))
           (table memberships_closure :id 103
             (column member (not-null int8)) (column grp (not-null int8)) (column paths (not-null int8))
             (primary-key member grp))
           (table docs :id 104 (column id (not-null int8)) (primary-key id))
           (entity User :table users :key id (attr dept))
           (entity Group :table groups :key id)
           (membership User Group :edges memberships (member grp) :closure memberships_closure (member grp paths)))",
    )
    .unwrap()
}

/// Open storage with users 1 and 2, user 1 in group 7, user 2 in group 8, and the policy table.
async fn setup(dir: &std::path::Path) -> FjallStorage {
    let storage = FjallStorage::open(
        dir,
        StorageOptions {
            durability: Durability::Buffered,
            gc_interval: None,
            ..StorageOptions::default()
        },
    )
    .unwrap();
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let int = |n: &str| ColumnDef::new(n, ColumnType::Int8);
    let tables = [
        (
            "users",
            vec![int("id"), ColumnDef::new("dept", ColumnType::Text)],
            vec!["id"],
        ),
        ("groups", vec![int("id")], vec!["id"]),
        (
            "memberships",
            vec![int("member"), int("grp")],
            vec!["member", "grp"],
        ),
        (
            "memberships_closure",
            vec![int("member"), int("grp"), int("paths").not_null()],
            vec!["member", "grp"],
        ),
        ("docs", vec![int("id")], vec!["id"]),
    ];
    for (name, columns, pk) in tables {
        let spec = TableSpec {
            name: name.into(),
            columns,
            primary_key: pk.into_iter().map(Into::into).collect(),
        };
        catalog::create_table(&txn, spec).await.unwrap();
    }
    let users = catalog::table(&txn, "users").await.unwrap().unwrap();
    rows::insert(
        &txn,
        &users,
        vec![Datum::Int8(1), Datum::Text("eng".into())],
    )
    .await
    .unwrap();
    rows::insert(&txn, &users, vec![Datum::Int8(2), Datum::Null])
        .await
        .unwrap();
    let closure = catalog::table(&txn, "memberships_closure")
        .await
        .unwrap()
        .unwrap();
    rows::insert(
        &txn,
        &closure,
        vec![Datum::Int8(1), Datum::Int8(7), Datum::Int8(1)],
    )
    .await
    .unwrap();
    rows::insert(
        &txn,
        &closure,
        vec![Datum::Int8(2), Datum::Int8(8), Datum::Int8(1)],
    )
    .await
    .unwrap();
    create_policy_table(&txn).await.unwrap();
    txn.commit().await.unwrap();
    storage
}

/// Delete `member`'s closure row for group `grp` in its own committed transaction.
async fn revoke(storage: &FjallStorage, member: i64, grp: i64) {
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let closure = catalog::table(&txn, "memberships_closure")
        .await
        .unwrap()
        .unwrap();
    assert!(
        rows::delete(&txn, &closure, &[Datum::Int8(member), Datum::Int8(grp)])
            .await
            .unwrap()
    );
    txn.commit().await.unwrap();
}

/// Insert a document, as an authorized write would.
async fn write_doc(txn: &interlaken_storage::Txn, id: i64) {
    let docs = catalog::table(txn, "docs").await.unwrap().unwrap();
    rows::insert(txn, &docs, vec![Datum::Int8(id)])
        .await
        .unwrap();
}

#[tokio::test]
/// Assert loading reads the principal's attributes and groups from the snapshot.
async fn loads_principal_data() {
    let dir = tempfile::tempdir().unwrap();
    let storage = setup(dir.path()).await;
    let catalog = ir_catalog();
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let loaded = load_principal(&txn, &catalog, &EntityUid::long("User", 1))
        .await
        .unwrap();
    assert_eq!(
        loaded.attributes,
        Some(vec![("dept".into(), SecValue::Str("eng".into()))])
    );
    assert_eq!(loaded.ancestors, vec![EntityUid::long("Group", 7)]);
    let missing = load_principal(&txn, &catalog, &EntityUid::long("User", 9))
        .await
        .unwrap();
    assert_eq!(missing.attributes, None);
}

#[tokio::test]
/// Assert a concurrent revocation fails a SERIALIZABLE writer that relied on the role, but not a REPEATABLE READ one.
async fn revocation_conflicts_under_serializable() {
    let dir = tempfile::tempdir().unwrap();
    let storage = setup(dir.path()).await;
    let catalog = ir_catalog();

    let txn = storage
        .begin(TxnOptions::new(Isolation::Serializable))
        .await
        .unwrap();
    let loaded = load_principal(&txn, &catalog, &EntityUid::long("User", 1))
        .await
        .unwrap();
    assert!(loaded.ancestors.contains(&EntityUid::long("Group", 7)));
    write_doc(&txn, 100).await;
    revoke(&storage, 1, 7).await;
    assert!(matches!(
        txn.commit().await,
        Err(Error::SerializationFailure(_))
    ));

    // Under REPEATABLE READ the same history commits: authorization used the snapshot.
    let dir = tempfile::tempdir().unwrap();
    let storage = setup(dir.path()).await;
    let txn = storage
        .begin(TxnOptions::new(Isolation::RepeatableRead))
        .await
        .unwrap();
    load_principal(&txn, &catalog, &EntityUid::long("User", 1))
        .await
        .unwrap();
    write_doc(&txn, 100).await;
    revoke(&storage, 1, 7).await;
    txn.commit().await.unwrap();
}

#[tokio::test]
/// Assert changes to other principals' memberships do not conflict.
async fn unrelated_revocations_do_not_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let storage = setup(dir.path()).await;
    let catalog = ir_catalog();
    let txn = storage
        .begin(TxnOptions::new(Isolation::Serializable))
        .await
        .unwrap();
    load_principal(&txn, &catalog, &EntityUid::long("User", 1))
        .await
        .unwrap();
    write_doc(&txn, 100).await;
    revoke(&storage, 2, 8).await;
    txn.commit().await.unwrap();
}

#[tokio::test]
/// Assert policies are versioned, validated, and a concurrent change fails writers at every isolation level.
async fn policy_changes_conflict_at_every_level() {
    let dir = tempfile::tempdir().unwrap();
    let storage = setup(dir.path()).await;
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    assert_eq!(load_policies(&txn).await.unwrap().version, 0);
    let v = store_policies(&txn, "(policies (permit (read) Doc true))")
        .await
        .unwrap();
    assert_eq!(v, 1);
    assert!(matches!(
        store_policies(&txn, "(policies (permit (fly) Doc true))").await,
        Err(EngineError::Policies(_))
    ));
    txn.commit().await.unwrap();

    for isolation in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        let txn = storage.begin(TxnOptions::new(isolation)).await.unwrap();
        let set = load_policies(&txn).await.unwrap();
        assert_eq!(set.policies.len(), 1);
        write_doc(&txn, 200 + set.version).await;
        let admin = storage.begin(TxnOptions::default()).await.unwrap();
        store_policies(&admin, "(policies)").await.unwrap();
        admin.commit().await.unwrap();
        assert!(
            matches!(txn.commit().await, Err(Error::SerializationFailure(_))),
            "{isolation:?}: a writer must not commit against a replaced policy set"
        );
        // Restore one policy for the next round.
        let admin = storage.begin(TxnOptions::default()).await.unwrap();
        store_policies(&admin, "(policies (permit (read) Doc true))")
            .await
            .unwrap();
        admin.commit().await.unwrap();
    }
}
