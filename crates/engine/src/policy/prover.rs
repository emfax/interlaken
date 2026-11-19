//! Provers: nodes that can prove policy changes (ADR-0030).

use std::future::Future;
use std::sync::Arc;

use interlaken_ir::catalog::Catalog;
use interlaken_storage::Txn;

use super::document::PolicyDocument;
use super::proposals::{Proposal, pending_proposals};
use super::tables::{load_policies, policy_settings};
use super::verify::{GateResult, record_verification, verifications};
use crate::error::EngineError;

/// Something that can run the proof gates on a policy change.
pub trait Prover {
    /// Return the name recorded with its results.
    fn name(&self) -> &str;

    /// Prove (or refute) the never-errors, no-broadening and invariants
    /// gates for replacing `current` with `proposal`'s policies.
    fn prove(
        &self,
        current: &PolicyDocument,
        proposal: &Proposal,
        catalog: &Arc<dyn Catalog>,
        app_context: &str,
    ) -> impl Future<Output = Result<Vec<GateResult>, EngineError>> + Send;
}

/// Run `prover` on every pending proposal based on the current version
/// that it has not yet verified, recording the results; return their IDs.
///
/// A prover node calls this whenever proposals appear. Results are
/// recorded in the transaction, so they replicate with the proposals.
///
/// # Errors
///
/// Returns an [`EngineError`] if storage or the prover fails.
pub async fn prove_pending<P: Prover>(
    txn: &Txn,
    catalog: Arc<dyn Catalog>,
    prover: &P,
) -> Result<Vec<i64>, EngineError> {
    let current = load_policies(txn).await?;
    let app_context = policy_settings(txn).await?.app_context;
    let mut proved = Vec::new();
    for proposal in pending_proposals(txn).await? {
        if proposal.base_version != current.version {
            continue;
        }
        let done = verifications(txn, proposal.id)
            .await?
            .iter()
            .any(|(v, r)| v == prover.name() && r.gate.is_proof());
        if done {
            continue;
        }
        let results = prover
            .prove(&current.document, &proposal, &catalog, &app_context)
            .await?;
        record_verification(txn, proposal.id, prover.name(), &results).await?;
        proved.push(proposal.id);
    }
    Ok(proved)
}
