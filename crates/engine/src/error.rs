//! Engine errors.

/// An error from the engine layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    /// A storage operation failed.
    #[error(transparent)]
    Storage(#[from] interlaken_storage::Error),
    /// Stored policies do not parse.
    #[error("stored policies are invalid: {0}")]
    Policies(#[from] interlaken_ir::text::ParseError),
    /// Policies or a policy-derived plan are invalid.
    #[error("invalid: {0}")]
    Invalid(String),
    /// No proposal has this ID.
    #[error("no policy proposal {0}")]
    ProposalNotFound(i64),
    /// The proposal was already activated or superseded.
    #[error("policy proposal {0} is not pending")]
    NotPending(i64),
    /// Another proposal was activated after this one was submitted.
    #[error(
        "policy proposal {proposal} is based on version {base}, but version {current} is active"
    )]
    Stale {
        /// The proposal.
        proposal: i64,
        /// The version it was based on.
        base: i64,
        /// The active version.
        current: i64,
    },
    /// No verifier has run the local gates yet.
    #[error("policy proposal {0} has not been verified")]
    NotVerified(i64),
    /// The cluster requires proof and no prover has run yet; the proposal waits.
    #[error("policy proposal {0} awaits a prover")]
    AwaitingProof(i64),
    /// A gate that cannot be overridden failed.
    #[error("policy proposal {proposal} failed: {detail}")]
    GateFailed {
        /// The proposal.
        proposal: i64,
        /// The failed gates.
        detail: String,
    },
    /// Gates flagged the change; someone must confirm it.
    #[error("policy proposal {proposal} needs confirmation: {detail}")]
    NeedsConfirmation {
        /// The proposal.
        proposal: i64,
        /// What the gates flagged.
        detail: String,
    },
    /// The IR catalog and the stored tables disagree.
    #[error("catalog mismatch: {0}")]
    Catalog(String),
}
