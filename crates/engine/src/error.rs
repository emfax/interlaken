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
    /// Running the statement's plan failed.
    #[error(transparent)]
    Exec(#[from] interlaken_exec::ExecError),
    /// The statement's text does not parse.
    #[error("syntax error: {0}")]
    Syntax(interlaken_ir::text::ParseError),
    /// Resolving the statement against the catalog failed.
    #[error(transparent)]
    Resolve(#[from] interlaken_ir::resolve::ResolveError),
    /// The statement's plan does not verify against the catalog.
    #[error(transparent)]
    Verify(#[from] interlaken_ir::verify::VerifyError),
    /// Applying policies to the statement failed (it fails closed).
    #[error(transparent)]
    Security(#[from] interlaken_ir::security::SecurityError),
    /// The principal lacks the privilege the operation needs.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The parameter values don't fit the parameters the statement declares.
    #[error(transparent)]
    Parameter(#[from] interlaken_ir::interp::ParamError),
    /// Revoking `admin` would leave no administrator.
    #[error("{0} is the last administrator; grant admin to another principal before revoking it")]
    LastAdmin(String),
    /// A row refers to a row that doesn't exist, through a foreign key (ADR-0035).
    #[error("foreign key violation: {0}")]
    ForeignKey(String),
    /// A DDL request is invalid.
    #[error("invalid definition: {0}")]
    InvalidDefinition(String),
    /// `begin` was called with a transaction already open.
    #[error("there is already a transaction in progress")]
    TransactionInProgress,
    /// A transaction operation was called with no transaction open.
    #[error("there is no transaction in progress")]
    NoTransaction,
    /// A table a change feed follows was dropped or replaced (ADR-0040).
    #[error("table {0} was dropped while a change feed followed it")]
    TableDropped(String),
    /// The statement is valid but can't be used here, such as DDL given
    /// to `prepare` (ADR-0043).
    #[error("not supported: {0}")]
    NotSupported(String),
    /// A statement of a script failed (ADR-0043); its SQLSTATE is the
    /// failure's.
    #[error("statement {index} ({form}) at {line}:{column}: {source}")]
    Script {
        /// The statement's 1-based position in the script.
        index: usize,
        /// Its head word, such as `create-index`.
        form: String,
        /// The line it starts on.
        line: u32,
        /// The column it starts at.
        column: u32,
        /// Why it failed.
        source: Box<EngineError>,
    },
    /// A wait for a commit to become visible on this node timed out (ADR-0040).
    #[error("commit {0} is not visible on this node yet")]
    NotVisible(interlaken_storage::Timestamp),
}

impl EngineError {
    /// Return the Postgres SQLSTATE code for this error.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_engine::EngineError;
    ///
    /// assert_eq!(EngineError::PermissionDenied("DDL".into()).sqlstate(), "42501");
    /// ```
    #[must_use]
    pub fn sqlstate(&self) -> &'static str {
        use interlaken_ir::interp::ParamError as P;
        use interlaken_ir::resolve::ResolveError as R;
        use interlaken_ir::security::SecurityError as S;
        use interlaken_ir::verify::VerifyError as V;
        use interlaken_storage::CatalogError as C;
        match self {
            Self::Exec(e) => e.sqlstate(),
            Self::Storage(interlaken_storage::Error::Catalog(c)) => match c {
                C::TableExists(_) => "42P07",
                C::IndexExists(_) => "42P07",
                C::ColumnExists { .. } => "42701",
                C::NoSuchTable(_) => "42P01",
                C::NoSuchIndex(_) => "42704",
                C::NoSuchColumn { .. } => "42703",
                C::InvalidDefinition(_) => "42P16",
                _ => "XX000",
            },
            Self::Storage(e) => e.sqlstate(),
            Self::Syntax(_) | Self::Policies(_) => "42601",
            Self::Resolve(e) => match e {
                R::UnknownTable(_) => "42P01",
                R::UnknownColumn(_) => "42703",
                R::AmbiguousColumn(_) => "42702",
                R::UnknownFunction(_) => "42883",
                R::AmbiguousFunction(_) => "42725",
                R::Type(_) => "42804",
                R::NotSupported(_) => "0A000",
                _ => "42000",
            },
            Self::Verify(e) => match e {
                V::StaleSchema { .. } => "0A000",
                V::Privileged(_) | V::SystemTableWrite(_) => "42501",
                _ => "42000",
            },
            Self::Security(S::ColumnDenied { .. }) | Self::PermissionDenied(_) => "42501",
            Self::Security(_) => "42501",
            Self::InvalidDefinition(_) => "42P16",
            Self::ForeignKey(_) => "23503",
            Self::Parameter(e) => match e {
                P::Count { .. } => "08P01",
                P::Type { .. } => "42804",
                P::Null { .. } => "22004",
            },
            Self::TransactionInProgress => "25001",
            Self::NoTransaction => "25P01",
            Self::NotVisible(_) => "57014",
            Self::NotSupported(_) => "0A000",
            Self::Script { source, .. } => source.sqlstate(),
            Self::TableDropped(_) => "42P01",
            Self::ProposalNotFound(_)
            | Self::NotPending(_)
            | Self::Stale { .. }
            | Self::LastAdmin(_) => "55000",
            Self::NotVerified(_)
            | Self::AwaitingProof(_)
            | Self::GateFailed { .. }
            | Self::NeedsConfirmation { .. } => "55000",
            Self::Invalid(_) | Self::Catalog(_) => "XX000",
        }
    }
}
