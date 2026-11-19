//! The verifier: the check every plan passes before it runs (ADR-0013).
//!
//! It accepts only resolved IR, checks it against the catalog, retypes
//! every expression with the strict rules, and rejects privileged
//! operators and system-table writes unless the engine's own passes
//! produced the plan.

mod check;
mod error;

pub use check::{VerifyOptions, column_types, expr_type, verify};
pub use error::VerifyError;
