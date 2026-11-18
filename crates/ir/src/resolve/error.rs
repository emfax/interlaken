//! Reasons resolution fails.

/// Why a plan could not be resolved.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ResolveError {
    /// No table has this name or ID.
    #[error("unknown table {0}")]
    UnknownTable(String),
    /// No visible column has this name, or no column has this binding.
    #[error("unknown column {0}")]
    UnknownColumn(String),
    /// A name matches more than one visible column.
    #[error("column reference {0} is ambiguous")]
    AmbiguousColumn(String),
    /// No function overload accepts these arguments.
    #[error("no function matches {0}")]
    UnknownFunction(String),
    /// Several function overloads accept these arguments equally well.
    #[error("function call {0} is ambiguous")]
    AmbiguousFunction(String),
    /// An expression or operator is not well typed.
    #[error("type error: {0}")]
    Type(String),
    /// The construct is valid but not supported yet.
    #[error("not supported: {0}")]
    NotSupported(String),
    /// The plan is malformed in some other way.
    #[error("invalid plan: {0}")]
    Invalid(String),
}
