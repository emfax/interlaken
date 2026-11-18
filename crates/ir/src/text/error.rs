//! Errors from reading the text format.

use super::sexp::Pos;

/// Text that is not valid IR, with the position of the problem.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{line}:{column}: {message}")]
pub struct ParseError {
    /// What is wrong.
    pub message: String,
    /// The 1-based line.
    pub line: u32,
    /// The 1-based column, in characters.
    pub column: u32,
}

impl ParseError {
    /// Construct an error at `pos`.
    pub(crate) fn at(pos: Pos, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            line: pos.line,
            column: pos.column,
        }
    }
}
