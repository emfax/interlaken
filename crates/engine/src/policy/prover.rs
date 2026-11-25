//! Provers: nodes that can prove policy changes (ADR-0030).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use interlaken_ir::catalog::Catalog;
use interlaken_storage::{Storage, Txn, TxnOptions};

use super::document::PolicyDocument;
use super::proposals::{Proposal, pending_proposals};
use super::tables::{PROPOSALS_TABLE, load_policies, policy_settings};
use super::verify::{GateResult, record_verification, verifications};
use crate::catalog::load_catalog;
use crate::error::EngineError;
use crate::feed::ChangeFilter;
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
    for mut proposal in pending_proposals(txn).await? {
        let scope = super::scopes::proposal_scope(txn, proposal.id).await?;
        let current_version = super::proposals::current_version(txn, scope.as_deref()).await?;
        if proposal.base_version != current_version {
            continue;
        }
        let done = verifications(txn, proposal.id)
            .await?
            .iter()
            .any(|(v, r)| v == prover.name() && r.gate.is_proof());
        if done {
            continue;
        }
        // A namespace's set is proved combined with the baseline, as it
        // would be enforced (ADR-0042).
        let mut document = current.document.clone();
        if let Some(scope) = scope
            .as_ref()
            .filter(|_| document.language == super::PolicyLanguage::Cedar)
        {
            let active = super::scopes::load_scope(txn, scope).await?;
            document.body = format!("{}\n{}", current.document.body, active.body);
            proposal.document.body =
                format!("{}\n{}", current.document.body, proposal.document.body);
        }
        let results = prover
            .prove(&document, &proposal, &catalog, &app_context)
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

/// Run `prover` on `engine`'s pending proposals whenever one is submitted
/// or changed, until `stop` resolves (ADR-0030 phase 2, ADR-0040).
///
/// On a cluster member, proposals submitted on any node arrive by
/// replication, and the results commit through the leader like any other
/// write, so one prover node serves the whole cluster. On a single node it
/// proves proposals submitted locally. The prover follows the proposals
/// table's change feed, so it wakes only when a proposal changes; without
/// a change log it checks every `poll` instead. Conflicts, a missing leader
/// and unknown commit outcomes are retried after `poll`.
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
    // Whether proposals may have changed since the last successful proof.
    let mut dirty = true;
    let mut feed = None;
    // Without a change log, poll the visible timestamp instead.
    let mut polling = false;
    let mut proved_at = None;
    loop {
        if !polling && feed.is_none() {
            let filter = ChangeFilter::tables([PROPOSALS_TABLE]).keys_only();
            match engine.changes(filter, None).await {
                Ok(opened) => {
                    feed = Some(opened);
                    // Anything submitted before the feed began.
                    dirty = true;
                }
                Err(error) if retryable(&error) => {}
                Err(error) => return Err(error),
            }
        }
        if dirty {
            let visible = engine.visible_ts();
            match prove_once(engine, prover).await {
                Ok(_) => {
                    dirty = false;
                    proved_at = Some(visible);
                }
                Err(error) if retryable(&error) => {}
                Err(error) => return Err(error),
            }
        }
        let wait = async {
            match feed.as_mut() {
                Some(open) if !dirty => match open.next().await {
                    Some(Ok(_)) => dirty = true,
                    Some(Err(EngineError::Storage(
                        interlaken_storage::Error::ChangesUnavailable,
                    ))) => {
                        feed = None;
                        polling = true;
                    }
                    // A gap: follow again from now, and prove what's pending.
                    Some(Err(_)) | None => feed = None,
                },
                _ => {
                    tokio::time::sleep(poll).await;
                    if polling && proved_at != Some(engine.visible_ts()) {
                        dirty = true;
                    }
                }
            }
        };
        tokio::select! {
            () = &mut stop => return Ok(()),
            () = wait => {}
        }
    }
}
