//! Reasons the verifier rejects a plan.

/// Why a plan failed verification.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// The plan still contains names, untyped NULLs or subqueries.
    #[error("not resolved: {0}")]
    Unresolved(String),
    /// The plan was resolved against a different catalog version.
    #[error("plan was resolved at schema version {plan:?}, but the catalog is at {catalog}")]
    StaleSchema {
        /// The plan's version, if it has one.
        plan: Option<u64>,
        /// The catalog's version.
        catalog: u64,
    },
    /// A table, column, function or binding the plan refers to does not exist.
    #[error("unknown reference: {0}")]
    UnknownReference(String),
    /// An operator only the engine may create came from a client.
    #[error("privileged operator from a client: {0}")]
    Privileged(String),
    /// A client plan writes a system table.
    #[error("clients may not write system table {0}")]
    SystemTableWrite(String),
    /// An expression or operator is not well typed.
    #[error("type error: {0}")]
    Type(String),
    /// The plan is malformed in some other way.
    #[error("malformed plan: {0}")]
    Structure(String),
}
