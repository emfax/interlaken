//! Executor errors.

use interlaken_ir::interp::EvalError;

/// Why a query failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExecError {
    /// Evaluating an expression raised an error (overflow, division by zero, ...).
    #[error(transparent)]
    Eval(#[from] EvalError),
    /// A storage operation failed.
    #[error(transparent)]
    Storage(#[from] interlaken_storage::Error),
    /// Building or slicing an Arrow batch failed; indicates an executor bug.
    #[error("arrow: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    /// The plan uses an operator or type this executor milestone does not cover.
    #[error("not supported by the executor yet: {0}")]
    Unsupported(String),
    /// The IR catalog and the stored tables disagree.
    #[error("catalog mismatch: {0}")]
    Catalog(String),
}
