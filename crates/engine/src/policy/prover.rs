//! Provers: nodes that can prove policy changes (ADR-0030).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use interlaken_ir::catalog::Catalog;
use interlaken_storage::{Storage, Txn, TxnOptions};

use super::document::PolicyDocument;
use super::proposals::{Proposal, pending_proposals};
use super::tables::{load_policies, policy_settings};
use super::verify::{GateResult, record_verification, verifications};
use crate::catalog::load_catalog;
use crate::error::EngineError;
use crate::session::Engine;

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

/// Return `true` for errors after which proving again later may succeed:
/// a conflict, no leader, or an unknown commit outcome. Proving is
/// idempotent, since a prover skips proposals it has already verified.
fn retryable(error: &EngineError) -> bool {
    matches!(error.sqlstate(), "40001" | "57P03" | "08007")
}

/// Prove what is pending on `engine` with `prover`, committing the results
/// if there were any; return the proposals proved.
async fn prove_once<P: Prover>(engine: &Engine, prover: &P) -> Result<Vec<i64>, EngineError> {
    let txn = engine.storage().begin(TxnOptions::default()).await?;
    let catalog: Arc<dyn Catalog> = Arc::new(load_catalog(&txn).await?);
    let proved = prove_pending(&txn, catalog, prover).await?;
    if proved.is_empty() {
        txn.abort();
    } else {
        txn.commit().await?;
    }
    Ok(proved)
}

/// Run `prover` on `engine`'s pending proposals whenever the node applies
/// new commits, until `stop` resolves (ADR-0030 phase 2).
///
/// On a cluster member, proposals submitted on any node arrive by
/// replication, and the results commit through the leader like any other
/// write, so one prover node serves the whole cluster. On a single node it
/// proves proposals submitted locally. The node is checked every `poll`:
/// a check that finds no new commits costs nothing, so the prover wakes
/// only for commits it hasn't seen. Conflicts, a missing leader and
/// unknown commit outcomes are retried on the next check.
///
/// # Errors
///
/// Returns the first error that retrying can't fix, such as a prover failure.
pub async fn watch_proposals<P: Prover>(
    engine: &Engine,
    prover: &P,
    poll: Duration,
    stop: impl Future<Output = ()>,
) -> Result<(), EngineError> {
    let mut stop = std::pin::pin!(stop);
    let mut seen = None;
    loop {
        tokio::select! {
            () = &mut stop => return Ok(()),
            () = tokio::time::sleep(poll) => {}
        }
        let visible = engine.storage().visible_ts();
        if seen == Some(visible) {
            continue;
        }
        match prove_once(engine, prover).await {
            Ok(_) => seen = Some(visible),
            Err(error) if retryable(&error) => {}
            Err(error) => return Err(error),
        }
    }
}
