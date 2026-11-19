//! Errors from security lowering.

/// Why a plan could not be secured. Security fails closed: an error means
/// the query does not run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SecurityError {
    /// A policy residual uses something lowering cannot express in IR.
    #[error("cannot lower policy condition: {0}")]
    Unsupported(String),
    /// The catalog lacks an entity, attribute or membership a policy uses.
    #[error("policy refers to {0}, which the catalog does not declare")]
    Undeclared(String),
    /// The query reads a column the principal may not read (ADR-0018).
    #[error("permission denied for column {column} of {table}")]
    ColumnDenied {
        /// The table.
        table: String,
        /// The column.
        column: String,
    },
    /// Resolving or verifying the secured plan failed.
    #[error("secured plan is invalid: {0}")]
    Invalid(String),
}
