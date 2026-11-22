//! Policy storage, proposals and verification (ADR-0019, ADR-0020, ADR-0030).
//!
//! The active policy set is one versioned row. A change is a proposal:
//! any node may submit one; verifiers record gate results (every node can
//! validate, lint and analyse impact on its data; prover nodes configured
//! with an SMT solver can also prove never-errors, no broadening and
//! declared invariants); and a proposal is activated only when the
//! cluster's verification mode is satisfied. The tables are ordinary
//! replicated tables, so the workflow is the same on one node and, once
//! replication exists, across many.

mod document;
mod proposals;
mod prover;
#[cfg(feature = "smt-prover")]
mod smt;
mod snapshot;
mod tables;
mod verify;

pub use document::{ActivePolicies, PolicyDocument, PolicyLanguage, build_provider};
pub use proposals::{
    Proposal, ProposalState, activate, pending_proposals, proposal, submit_proposal,
};
pub use prover::{Prover, prove_pending, watch_proposals};
#[cfg(feature = "smt-prover")]
pub use smt::SmtProver;
pub use snapshot::entity_snapshot;
pub use tables::{
    POLICY_TABLE, PolicySettings, VerificationMode, create_policy_tables, load_policies,
    policy_settings, set_policy_settings,
};
pub use verify::{Gate, GateResult, record_verification, verifications, verify_locally};
