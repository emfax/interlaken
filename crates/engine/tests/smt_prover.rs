//! The SMT prover (ADR-0020 phase 2, ADR-0030, milestone I5).
//!
//! Needs the `smt-prover` feature and a cvc5 1.3.1 executable, found
//! through the `CVC5` environment variable or under `target/` (as
//! `target/cvc5`, or unpacked from the release zip in
//! `target/cvc5-download`). Without one, the tests note it and pass.

#![cfg(feature = "smt-prover")]

use std::path::{Path, PathBuf};

use interlaken_engine::policy::{
    Gate, PolicyDocument, PolicyLanguage, Proposal, ProposalState, Prover, SmtProver,
};

mod support;

use support::shared_catalog;

/// Find a cvc5 executable, or `None`.
fn cvc5() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CVC5") {
        return Some(PathBuf::from(path));
    }
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let direct = target.join("cvc5");
    if direct.is_file() {
        return Some(direct);
    }
    // The release zip unpacks to cvc5-Linux-x86_64-static/bin/cvc5.
    let unpacked = target.join("cvc5-download/cvc5-Linux-x86_64-static/bin/cvc5");
    unpacked.is_file().then_some(unpacked)
}

/// Return Cedar policies as a document.
fn cedar(body: &str) -> PolicyDocument {
    PolicyDocument {
        language: PolicyLanguage::Cedar,
        body: body.into(),
    }
}

/// Return a pending proposal of `body` with `invariants`.
fn proposal(body: &str, invariants: &str) -> Proposal {
    Proposal {
        id: 1,
        base_version: 0,
        document: cedar(body),
        invariants: invariants.into(),
        proposer: "alice".into(),
        state: ProposalState::Pending,
        activated_version: None,
        confirmed_by: None,
    }
}

/// Owners read their documents.
const OWNERS: &str = r#"permit (principal, action == Action::"read", resource is Doc) when { resource.owner == principal };"#;

/// Owners, and members of group 7, read documents.
const OWNERS_OR_GROUP: &str = r#"permit (principal, action == Action::"read", resource is Doc) when { resource.owner == principal || principal in Group::"7" };"#;

/// Prove replacing `current` with `proposed`; return (gate, passed, detail) triples.
async fn prove(
    path: &Path,
    current: &str,
    proposed: &str,
    invariants: &str,
) -> Vec<(Gate, bool, String)> {
    let prover = SmtProver::new("prover-1", path);
    prover
        .prove(
            &cedar(current),
            &proposal(proposed, invariants),
            &shared_catalog(),
            "{}",
        )
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.gate, r.passed, r.detail))
        .collect()
}

/// Return whether `gate` passed in `results`, and its detail.
fn gate(results: &[(Gate, bool, String)], gate: Gate) -> (bool, String) {
    let (_, passed, detail) = results
        .iter()
        .find(|(g, ..)| *g == gate)
        .expect("gate reported");
    (*passed, detail.clone())
}

#[tokio::test]
/// Assert narrowing proves clean, broadening yields a counterexample, and errors and invariant violations are found.
async fn proofs_and_counterexamples() {
    let Some(path) = cvc5() else {
        eprintln!("cvc5 not found (set CVC5 or unpack it under target/); SMT prover tests skipped");
        return;
    };
    // Narrowing: from owners-or-group to owners only.
    let narrowing = prove(&path, OWNERS_OR_GROUP, OWNERS, "").await;
    assert!(
        narrowing.iter().all(|(_, passed, _)| *passed),
        "{narrowing:?}"
    );

    // Broadening: the counterexample is a member of group 7 reading someone else's document.
    let broadening = prove(&path, OWNERS, OWNERS_OR_GROUP, "").await;
    let (passed, detail) = gate(&broadening, Gate::NoBroadening);
    assert!(!passed, "broadening must be found");
    eprintln!("broadening counterexample: {detail}");
    assert!(detail.contains("Group") && detail.contains("7"), "{detail}");

    // A forbid that can overflow.
    let overflow = format!(
        r#"{OWNERS} forbid (principal, action == Action::"read", resource is Doc) when {{ resource has secret && resource.secret + 1 > 3 }};"#
    );
    let (passed, detail) = gate(
        &prove(&path, OWNERS, &overflow, "").await,
        Gate::NeverErrors,
    );
    assert!(!passed && detail.contains("raises an error"), "{detail}");

    // An invariant: only owners may read documents.
    let (passed, _) = gate(
        &prove(&path, OWNERS, OWNERS_OR_GROUP, OWNERS).await,
        Gate::Invariants,
    );
    assert!(
        !passed,
        "group members reading others' documents violates the invariant"
    );
    let (passed, _) = gate(
        &prove(&path, OWNERS_OR_GROUP, OWNERS, OWNERS).await,
        Gate::Invariants,
    );
    assert!(passed);
}

#[tokio::test]
/// Assert non-Cedar policies cannot be proved, so proof-required clusters cannot activate them.
async fn rls_policies_cannot_be_proved() {
    let prover = SmtProver::new("prover-1", "/nonexistent/cvc5");
    let rls = Proposal {
        document: PolicyDocument {
            language: PolicyLanguage::Rls,
            body: "(policies)".into(),
        },
        ..proposal("", "")
    };
    let results = prover
        .prove(&cedar(""), &rls, &shared_catalog(), "{}")
        .await
        .unwrap();
    assert!(results.iter().all(|r| !r.passed && r.gate.is_proof()));
}
