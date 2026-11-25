//! Gates and recorded verifications (ADR-0020, ADR-0030).

use std::sync::Arc;

use interlaken_ir::catalog::Catalog;
use interlaken_ir::security::{SecurityContext, impact_analysis};
use interlaken_storage::Txn;
use interlaken_storage::encoding::Datum;
use interlaken_storage::rows::{self, RowScan};

use super::proposals::proposal;
use super::snapshot::entity_snapshot;
use super::tables::{VERIFICATIONS_TABLE, int_of, policy_settings, table, text_of};
use crate::error::EngineError;

/// A check a policy change must pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gate {
    /// The policies parse and type-check (Cedar: strict validation against the generated schema).
    Validation,
    /// No forbid condition can raise an error (a syntactic check).
    Lint,
    /// On current data, the change allows nothing that was denied.
    Impact,
    /// Proved: no policy raises an error on any well-formed request.
    NeverErrors,
    /// Proved: the new set allows no request the old set denied.
    NoBroadening,
    /// Proved: the new set allows only what every declared invariant allows.
    Invariants,
}

impl Gate {
    /// Every gate.
    pub const ALL: [Self; 6] = [
        Self::Validation,
        Self::Lint,
        Self::Impact,
        Self::NeverErrors,
        Self::NoBroadening,
        Self::Invariants,
    ];

    /// Return the stored name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Validation => "validation",
            Self::Lint => "lint",
            Self::Impact => "impact",
            Self::NeverErrors => "never-errors",
            Self::NoBroadening => "no-broadening",
            Self::Invariants => "invariants",
        }
    }

    /// Parse a stored name.
    fn from_name(name: &str) -> Result<Self, EngineError> {
        Self::ALL
            .into_iter()
            .find(|g| g.name() == name)
            .ok_or_else(|| EngineError::Catalog(format!("unknown gate {name}")))
    }

    /// Return `true` if failing this gate blocks activation outright; the
    /// others block only until someone confirms the change.
    #[must_use]
    pub const fn is_hard(self) -> bool {
        matches!(
            self,
            Self::Validation | Self::NeverErrors | Self::Invariants
        )
    }

    /// Return `true` for the gates only a prover can run.
    #[must_use]
    pub const fn is_proof(self) -> bool {
        matches!(
            self,
            Self::NeverErrors | Self::NoBroadening | Self::Invariants
        )
    }
}

/// One gate's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateResult {
    /// The gate.
    pub gate: Gate,
    /// Whether it passed.
    pub passed: bool,
    /// What was found: an impact summary, lint warnings, or a counterexample.
    pub detail: String,
}

impl GateResult {
    /// Construct a result.
    #[must_use]
    pub fn new(gate: Gate, passed: bool, detail: impl Into<String>) -> Self {
        Self {
            gate,
            passed,
            detail: detail.into(),
        }
    }
}

/// Record `verifier`'s gate results for `proposal`, replacing earlier ones for the same gates.
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails.
pub async fn record_verification(
    txn: &Txn,
    proposal: i64,
    verifier: &str,
    results: &[GateResult],
) -> Result<(), EngineError> {
    let table = table(txn, VERIFICATIONS_TABLE).await?;
    for r in results {
        let pk = [
            Datum::Int8(proposal),
            Datum::Text(verifier.into()),
            Datum::Text(r.gate.name().into()),
        ];
        let row = vec![
            pk[0].clone(),
            pk[1].clone(),
            pk[2].clone(),
            Datum::Bool(r.passed),
            Datum::Text(r.detail.clone()),
        ];
        if !rows::update(txn, &table, &pk, row.clone()).await? {
            rows::insert(txn, &table, row).await?;
        }
    }
    txn.next_statement();
    Ok(())
}

/// Return every recorded gate result for `proposal`, with its verifier.
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails or a row is malformed.
pub async fn verifications(
    txn: &Txn,
    proposal: i64,
) -> Result<Vec<(String, GateResult)>, EngineError> {
    let table = table(txn, VERIFICATIONS_TABLE).await?;
    let mut out = Vec::new();
    let mut scan = RowScan::prefix(vec![Datum::Int8(proposal)]);
    loop {
        let batch = rows::scan(txn, &table, &scan).await?;
        for row in &batch.rows {
            debug_assert_eq!(int_of(&row[0])?, proposal);
            let passed = matches!(row[3], Datum::Bool(true));
            out.push((
                text_of(&row[1])?.to_owned(),
                GateResult::new(
                    Gate::from_name(text_of(&row[2])?)?,
                    passed,
                    text_of(&row[4])?,
                ),
            ));
        }
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => break,
        }
    }
    Ok(out)
}

/// Run the gates every node can run on `proposal` and record them as `verifier`.
///
/// Validation, the forbid lint, and impact analysis against this node's
/// data, with `base` as the context (its principal is replaced for each
/// principal analysed). Impact analysis passes if nothing denied today
/// would be allowed.
///
/// # Errors
///
/// Returns an [`EngineError`] if the proposal is missing or storage fails.
/// Invalid policies are a failed gate, not an error.
pub async fn verify_locally(
    txn: &Txn,
    catalog: Arc<dyn Catalog>,
    proposal_id: i64,
    verifier: &str,
    base: &SecurityContext,
) -> Result<Vec<GateResult>, EngineError> {
    let proposal = proposal(txn, proposal_id)
        .await?
        .ok_or(EngineError::ProposalNotFound(proposal_id))?;
    let settings = policy_settings(txn).await?;
    let (current, scopes) = super::scopes::load_all(txn).await?;
    let scope = super::scopes::proposal_scope(txn, proposal_id).await?;
    // The proposal against everything else that's active: the baseline it
    // replaces, or the baseline with its namespace's set (ADR-0042).
    let replacing = match &scope {
        None => super::scopes::Replacing::Baseline(&proposal.document),
        Some(scope) => super::scopes::Replacing::Scope(scope, &proposal.document.body),
    };
    let proposed = match &scope {
        Some(scope) => super::scopes::check_confined(
            proposal.document.language,
            scope,
            &proposal.document.body,
        ),
        None => Ok(()),
    }
    .and_then(|()| {
        super::scopes::effective(
            &current,
            &scopes,
            replacing,
            catalog.clone(),
            &settings.app_context,
        )
    });
    let mut results = Vec::new();
    match proposed {
        Err(e) => results.push(GateResult::new(Gate::Validation, false, e.to_string())),
        Ok(proposed) => {
            results.push(GateResult::new(Gate::Validation, true, "valid"));
            let warnings = proposed.lint();
            results.push(GateResult::new(
                Gate::Lint,
                warnings.is_empty(),
                warnings.join("; "),
            ));
            let current_provider = super::scopes::effective(
                &current,
                &scopes,
                super::scopes::Replacing::Nothing,
                catalog.clone(),
                &settings.app_context,
            )?;
            let data = entity_snapshot(txn, catalog.as_ref()).await?;
            let report = impact_analysis(
                current_provider.as_ref(),
                proposed.as_ref(),
                catalog.as_ref(),
                &data,
                base,
            )
            .map_err(|e| EngineError::Invalid(e.to_string()))?;
            results.push(GateResult::new(
                Gate::Impact,
                report.newly_allowed.is_empty(),
                report.summary(10),
            ));
        }
    }
    record_verification(txn, proposal_id, verifier, &results).await?;
    Ok(results)
}
