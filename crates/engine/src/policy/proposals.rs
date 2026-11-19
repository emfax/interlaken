//! Proposed policy changes and their activation (ADR-0030).

use interlaken_storage::Txn;
use interlaken_storage::encoding::Datum;
use interlaken_storage::rows::{self, RowScan};

use super::document::{PolicyDocument, PolicyLanguage};
use super::tables::{
    PROPOSALS_TABLE, VerificationMode, int_of, load_policies, policy_settings, table, text_of,
    write_policies,
};
use super::verify::{Gate, verifications};
use crate::error::EngineError;

/// Where a proposal is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalState {
    /// Submitted, awaiting verification or activation.
    Pending,
    /// Became the active policy set.
    Activated,
    /// Another proposal was activated first, so its base is out of date.
    Superseded,
}

impl ProposalState {
    /// Return the stored name.
    const fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Activated => "activated",
            Self::Superseded => "superseded",
        }
    }

    /// Parse a stored name.
    fn from_name(name: &str) -> Result<Self, EngineError> {
        match name {
            "pending" => Ok(Self::Pending),
            "activated" => Ok(Self::Activated),
            "superseded" => Ok(Self::Superseded),
            other => Err(EngineError::Catalog(format!(
                "unknown proposal state {other}"
            ))),
        }
    }
}

/// A proposed policy change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    /// The proposal's ID.
    pub id: i64,
    /// The policy version it replaces.
    pub base_version: i64,
    /// The proposed policies.
    pub document: PolicyDocument,
    /// Declared invariants: Cedar policies describing the most the new set may allow.
    pub invariants: String,
    /// Who proposed it.
    pub proposer: String,
    /// Its state.
    pub state: ProposalState,
    /// The version it became, once activated.
    pub activated_version: Option<i64>,
    /// Who confirmed it, if a gate needed confirmation.
    pub confirmed_by: Option<String>,
}

/// Decode a proposal row.
fn decode(row: &[Datum]) -> Result<Proposal, EngineError> {
    let optional_text = |d: &Datum| -> Result<Option<String>, EngineError> {
        Ok(if d.is_null() {
            None
        } else {
            Some(text_of(d)?.to_owned())
        })
    };
    Ok(Proposal {
        id: int_of(&row[0])?,
        base_version: int_of(&row[1])?,
        document: PolicyDocument {
            language: PolicyLanguage::from_name(text_of(&row[2])?)?,
            body: text_of(&row[3])?.to_owned(),
        },
        invariants: text_of(&row[4])?.to_owned(),
        proposer: text_of(&row[5])?.to_owned(),
        state: ProposalState::from_name(text_of(&row[6])?)?,
        activated_version: if row[7].is_null() {
            None
        } else {
            Some(int_of(&row[7])?)
        },
        confirmed_by: optional_text(&row[8])?,
    })
}

/// Encode a proposal row.
fn encode(p: &Proposal) -> Vec<Datum> {
    vec![
        Datum::Int8(p.id),
        Datum::Int8(p.base_version),
        Datum::Text(p.document.language.name().into()),
        Datum::Text(p.document.body.clone()),
        Datum::Text(p.invariants.clone()),
        Datum::Text(p.proposer.clone()),
        Datum::Text(p.state.name().into()),
        p.activated_version.map_or(Datum::Null, Datum::Int8),
        p.confirmed_by.clone().map_or(Datum::Null, Datum::Text),
    ]
}

/// Return every proposal.
async fn all(txn: &Txn) -> Result<Vec<Proposal>, EngineError> {
    let table = table(txn, PROPOSALS_TABLE).await?;
    let mut out = Vec::new();
    let mut scan = RowScan::all();
    loop {
        let batch = rows::scan(txn, &table, &scan).await?;
        for row in &batch.rows {
            out.push(decode(row)?);
        }
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => break,
        }
    }
    Ok(out)
}

/// Submit a proposed policy change against the current version; return its ID.
///
/// Nothing changes until it is activated. `invariants` is Cedar policy text
/// describing the most the new set may allow (empty for none).
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails.
pub async fn submit_proposal(
    txn: &Txn,
    document: &PolicyDocument,
    invariants: &str,
    proposer: &str,
) -> Result<i64, EngineError> {
    let base_version = load_policies(txn).await?.version;
    let id = all(txn).await?.iter().map(|p| p.id).max().unwrap_or(0) + 1;
    let proposal = Proposal {
        id,
        base_version,
        document: document.clone(),
        invariants: invariants.into(),
        proposer: proposer.into(),
        state: ProposalState::Pending,
        activated_version: None,
        confirmed_by: None,
    };
    rows::insert(txn, &table(txn, PROPOSALS_TABLE).await?, encode(&proposal)).await?;
    // Each policy operation is a statement: later ones see its writes.
    txn.next_statement();
    Ok(id)
}

/// Read one proposal.
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails.
pub async fn proposal(txn: &Txn, id: i64) -> Result<Option<Proposal>, EngineError> {
    let table = table(txn, PROPOSALS_TABLE).await?;
    rows::get(txn, &table, &[Datum::Int8(id)])
        .await?
        .map(|r| decode(&r))
        .transpose()
}

/// Return the pending proposals, oldest first.
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails.
pub async fn pending_proposals(txn: &Txn) -> Result<Vec<Proposal>, EngineError> {
    Ok(all(txn)
        .await?
        .into_iter()
        .filter(|p| p.state == ProposalState::Pending)
        .collect())
}

/// Make proposal `id` the active policy set, if the verification mode allows; return the new version.
///
/// The proposal must be pending and based on the current version. Its
/// recorded results must include a passed validation gate; in
/// [`VerificationMode::ProofRequired`] they must also include every proof
/// gate, so the proposal waits until a prover has run. A failed hard gate
/// (validation, never-errors, invariants) blocks activation; any other
/// failed gate (lint, impact, no-broadening) requires `confirmed_by`.
/// Activation supersedes every other pending proposal.
///
/// # Errors
///
/// Returns [`EngineError::ProposalNotFound`], [`EngineError::NotPending`],
/// [`EngineError::Stale`], [`EngineError::NotVerified`],
/// [`EngineError::AwaitingProof`], [`EngineError::GateFailed`] or
/// [`EngineError::NeedsConfirmation`], or a storage error.
pub async fn activate(txn: &Txn, id: i64, confirmed_by: Option<&str>) -> Result<i64, EngineError> {
    let mut proposal = proposal(txn, id)
        .await?
        .ok_or(EngineError::ProposalNotFound(id))?;
    if proposal.state != ProposalState::Pending {
        return Err(EngineError::NotPending(id));
    }
    let current = load_policies(txn).await?;
    if proposal.base_version != current.version {
        return Err(EngineError::Stale {
            proposal: id,
            base: proposal.base_version,
            current: current.version,
        });
    }
    let results = verifications(txn, id).await?;
    let has = |gate: Gate| results.iter().any(|(_, r)| r.gate == gate);
    if !has(Gate::Validation) {
        return Err(EngineError::NotVerified(id));
    }
    if policy_settings(txn).await?.mode == VerificationMode::ProofRequired
        && !Gate::ALL.iter().filter(|g| g.is_proof()).all(|g| has(*g))
    {
        return Err(EngineError::AwaitingProof(id));
    }
    let failed: Vec<String> = results
        .iter()
        .filter(|(_, r)| !r.passed)
        .map(|(verifier, r)| format!("{} ({verifier}): {}", r.gate.name(), r.detail))
        .collect();
    if results.iter().any(|(_, r)| !r.passed && r.gate.is_hard()) {
        return Err(EngineError::GateFailed {
            proposal: id,
            detail: failed.join("; "),
        });
    }
    if !failed.is_empty() && confirmed_by.is_none() {
        return Err(EngineError::NeedsConfirmation {
            proposal: id,
            detail: failed.join("; "),
        });
    }
    let version = current.version + 1;
    write_policies(txn, version, &proposal.document).await?;
    let table = table(txn, PROPOSALS_TABLE).await?;
    for mut other in pending_proposals(txn).await? {
        if other.id != id {
            other.state = ProposalState::Superseded;
            rows::update(txn, &table, &[Datum::Int8(other.id)], encode(&other)).await?;
        }
    }
    proposal.state = ProposalState::Activated;
    proposal.activated_version = Some(version);
    proposal.confirmed_by = confirmed_by.map(str::to_owned);
    rows::update(txn, &table, &[Datum::Int8(id)], encode(&proposal)).await?;
    txn.next_statement();
    Ok(version)
}
