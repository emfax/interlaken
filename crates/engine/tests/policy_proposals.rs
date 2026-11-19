//! Policy change proposals and verification (ADR-0030, milestone I5).
//!
//! A proposal changes nothing until activated. Activation needs a passed
//! validation gate; in proof-required mode it waits for every proof gate;
//! a failed hard gate blocks it, and any other flagged gate needs a named
//! confirmation. A fake prover stands in for the SMT prover, whose own
//! tests need cvc5.

use std::sync::Arc;

use interlaken_engine::EngineError;
use interlaken_engine::policy::{
    Gate, GateResult, PolicyDocument, PolicyLanguage, Proposal, ProposalState, Prover,
    VerificationMode, activate, load_policies, pending_proposals, proposal, prove_pending,
    submit_proposal, verifications, verify_locally,
};
use interlaken_ir::catalog::Catalog;
use interlaken_ir::security::{EntityUid, SecurityContext};
use interlaken_storage::{FjallStorage, Storage, Txn, TxnOptions};

mod support;

use support::{setup, shared_catalog};

/// Return Cedar policies as a document.
fn cedar(body: &str) -> PolicyDocument {
    PolicyDocument {
        language: PolicyLanguage::Cedar,
        body: body.into(),
    }
}

/// Owners read their documents.
const OWNERS: &str = r#"permit (principal, action == Action::"read", resource is Doc) when { resource.owner == principal };"#;

/// Everyone reads every document.
const EVERYONE: &str = r#"permit (principal, action == Action::"read", resource is Doc);"#;

/// Submit `document` and run the local gates; return the proposal ID.
async fn submit_and_verify(txn: &Txn, document: &PolicyDocument) -> i64 {
    let id = submit_proposal(txn, document, "", "alice").await.unwrap();
    verify_locally(
        txn,
        shared_catalog(),
        id,
        "node-a",
        &SecurityContext::new(EntityUid::long("User", 1)),
    )
    .await
    .unwrap();
    id
}

/// Begin a read-write transaction.
async fn begin(storage: &FjallStorage) -> Txn {
    storage.begin(TxnOptions::default()).await.unwrap()
}

#[tokio::test]
/// Assert narrowing changes activate directly, broadening ones need confirmation, and invalid ones never do.
async fn analysis_sufficient_activation() {
    let dir = tempfile::tempdir().unwrap();
    let storage = setup(dir.path(), VerificationMode::AnalysisSufficient).await;
    let txn = begin(&storage).await;

    // From deny-everything to owners: impact analysis flags users newly reading their documents.
    let owners = submit_and_verify(&txn, &cedar(OWNERS)).await;
    assert_eq!(
        load_policies(&txn).await.unwrap().version,
        0,
        "nothing changes before activation"
    );
    let Err(EngineError::NeedsConfirmation { detail, .. }) = activate(&txn, owners, None).await
    else {
        panic!("broadening must need confirmation");
    };
    assert!(
        detail.contains("impact") && detail.contains("newly allowed"),
        "{detail}"
    );
    assert_eq!(activate(&txn, owners, Some("bob")).await.unwrap(), 1);
    let activated = proposal(&txn, owners).await.unwrap().unwrap();
    assert_eq!(
        (
            activated.state,
            activated.activated_version,
            activated.confirmed_by.as_deref()
        ),
        (ProposalState::Activated, Some(1), Some("bob"))
    );

    // Narrowing back to nothing allows nothing new: no confirmation needed.
    let narrow = submit_and_verify(&txn, &cedar("")).await;
    assert_eq!(activate(&txn, narrow, None).await.unwrap(), 2);

    // Invalid policies fail validation, which no confirmation overrides.
    let invalid = submit_and_verify(&txn, &cedar(r#"permit (principal, action == Action::"read", resource is Doc) when { resource.nope };"#)).await;
    assert!(matches!(
        activate(&txn, invalid, Some("bob")).await,
        Err(EngineError::GateFailed { .. })
    ));

    // A forbid that can raise an error is flagged by the lint.
    let risky = submit_and_verify(
        &txn,
        &cedar(r#"forbid (principal, action == Action::"read", resource is Doc) when { resource has secret && resource.secret + 1 > 3 };"#),
    )
    .await;
    let Err(EngineError::NeedsConfirmation { detail, .. }) = activate(&txn, risky, None).await
    else {
        panic!("a risky forbid must need confirmation");
    };
    assert!(
        detail.contains("lint") && detail.contains("overflow"),
        "{detail}"
    );
    txn.commit().await.unwrap();
}

#[tokio::test]
/// Assert unverified proposals cannot activate and activation supersedes the other pending proposals.
async fn verification_and_superseding() {
    let dir = tempfile::tempdir().unwrap();
    let storage = setup(dir.path(), VerificationMode::AnalysisSufficient).await;
    let txn = begin(&storage).await;
    let unverified = submit_proposal(&txn, &cedar(OWNERS), "", "alice")
        .await
        .unwrap();
    assert!(matches!(
        activate(&txn, unverified, Some("bob")).await,
        Err(EngineError::NotVerified(_))
    ));
    let a = submit_and_verify(&txn, &cedar(OWNERS)).await;
    let b = submit_and_verify(&txn, &cedar(EVERYONE)).await;
    activate(&txn, a, Some("bob")).await.unwrap();
    assert_eq!(
        proposal(&txn, b).await.unwrap().unwrap().state,
        ProposalState::Superseded
    );
    assert!(matches!(
        activate(&txn, b, Some("bob")).await,
        Err(EngineError::NotPending(_))
    ));
    assert!(pending_proposals(&txn).await.unwrap().is_empty());
    txn.commit().await.unwrap();
}

/// A prover that records fixed results.
struct FakeProver {
    /// Whether never-errors passes.
    never_errors: bool,
}

impl Prover for FakeProver {
    /// Return the prover's name.
    fn name(&self) -> &str {
        "prover-1"
    }

    /// Return fixed results.
    async fn prove(
        &self,
        _current: &PolicyDocument,
        _proposal: &Proposal,
        _catalog: &Arc<dyn Catalog>,
        _app_context: &str,
    ) -> Result<Vec<GateResult>, EngineError> {
        Ok(vec![
            GateResult::new(Gate::NeverErrors, self.never_errors, "fake"),
            GateResult::new(Gate::NoBroadening, false, "fake counterexample"),
            GateResult::new(Gate::Invariants, true, "fake"),
        ])
    }
}

#[tokio::test]
/// Assert proof-required proposals wait for a prover, then follow its verdict.
async fn proof_required_waits_for_a_prover() {
    let dir = tempfile::tempdir().unwrap();
    let storage = setup(dir.path(), VerificationMode::ProofRequired).await;

    // Any node submits and runs the local gates; activation waits.
    let txn = begin(&storage).await;
    let id = submit_and_verify(&txn, &cedar(OWNERS)).await;
    assert!(matches!(
        activate(&txn, id, Some("bob")).await,
        Err(EngineError::AwaitingProof(_))
    ));
    txn.commit().await.unwrap();

    // A prover node, in its own transaction, proves what is pending.
    let prover_txn = begin(&storage).await;
    let proved = prove_pending(
        &prover_txn,
        shared_catalog(),
        &FakeProver { never_errors: true },
    )
    .await
    .unwrap();
    assert_eq!(proved, [id]);
    assert!(
        prove_pending(
            &prover_txn,
            shared_catalog(),
            &FakeProver { never_errors: true }
        )
        .await
        .unwrap()
        .is_empty(),
        "proved once"
    );
    prover_txn.commit().await.unwrap();

    // The prover found broadening, which needs confirmation; then it activates.
    let txn = begin(&storage).await;
    let gates: Vec<(String, Gate, bool)> = verifications(&txn, id)
        .await
        .unwrap()
        .into_iter()
        .map(|(v, r)| (v, r.gate, r.passed))
        .collect();
    assert!(
        gates.contains(&("prover-1".into(), Gate::NoBroadening, false)),
        "{gates:?}"
    );
    assert!(matches!(
        activate(&txn, id, None).await,
        Err(EngineError::NeedsConfirmation { .. })
    ));
    assert_eq!(activate(&txn, id, Some("bob")).await.unwrap(), 1);

    // A proposal the prover refutes (it can raise an error) never activates.
    let refuted = submit_and_verify(&txn, &cedar(EVERYONE)).await;
    prove_pending(
        &txn,
        shared_catalog(),
        &FakeProver {
            never_errors: false,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        activate(&txn, refuted, Some("bob")).await,
        Err(EngineError::GateFailed { .. })
    ));
    txn.commit().await.unwrap();
}
