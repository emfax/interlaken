//! Transactional authorization (ADR-0019 phase 1, ADR-0021 phase 4, milestone I4).
//!
//! The principal's memberships and the policy set are read through the
//! transaction. A role revoked by a concurrent transaction therefore makes
//! a SERIALIZABLE transaction that relied on it fail at commit, and a
//! concurrent policy change makes a writing transaction fail at every
//! isolation level.

use interlaken_engine::load_principal;
use interlaken_engine::policy::{
    PolicyDocument, PolicyLanguage, VerificationMode, activate, load_policies, submit_proposal,
    verify_locally,
};
use interlaken_ir::security::{EntityUid, SecValue, SecurityContext};
use interlaken_storage::catalog;
use interlaken_storage::encoding::Datum;
use interlaken_storage::{Error, FjallStorage, Isolation, Storage, TxnOptions, rows};

mod support;

use support::ir_catalog;

/// Open the shared fixture in analysis-sufficient mode.
async fn setup(dir: &std::path::Path) -> FjallStorage {
    support::setup(dir, VerificationMode::AnalysisSufficient).await
}

/// Make `body` (RLS policies) the active policy set through the proposal workflow, as an admin.
async fn activate_rls(storage: &FjallStorage, body: &str) -> i64 {
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let document = PolicyDocument {
        language: PolicyLanguage::Rls,
        body: body.into(),
    };
    let id = submit_proposal(&txn, &document, "", "admin").await.unwrap();
    let base = SecurityContext::new(EntityUid::long("User", 1));
    verify_locally(&txn, support::shared_catalog(), id, "local", &base)
        .await
        .unwrap();
    let version = activate(&txn, id, Some("admin")).await.unwrap();
    txn.commit().await.unwrap();
    version
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
    rows::insert(
        txn,
        &docs,
        vec![Datum::Int8(id), Datum::Int8(1), Datum::Null],
    )
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
/// Assert a concurrent policy activation fails writers that loaded the old set, at every isolation level.
async fn policy_changes_conflict_at_every_level() {
    let dir = tempfile::tempdir().unwrap();
    let storage = setup(dir.path()).await;
    assert_eq!(
        activate_rls(&storage, "(policies (permit (read) Doc true))").await,
        1
    );
    for isolation in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        let txn = storage.begin(TxnOptions::new(isolation)).await.unwrap();
        let active = load_policies(&txn).await.unwrap();
        write_doc(&txn, 200 + active.version).await;
        let body = if active.version % 2 == 1 {
            "(policies)"
        } else {
            "(policies (permit (read) Doc true))"
        };
        activate_rls(&storage, body).await;
        assert!(
            matches!(txn.commit().await, Err(Error::SerializationFailure(_))),
            "{isolation:?}: a writer must not commit against a replaced policy set"
        );
    }
}
