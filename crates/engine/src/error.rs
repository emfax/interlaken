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
    /// The IR catalog and the stored tables disagree.
    #[error("catalog mismatch: {0}")]
    Catalog(String),
}
