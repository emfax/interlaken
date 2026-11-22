//! The rule-based optimizer (ADR-0014).
//!
//! Rewrites run in fixed phases, each respecting the security barrier:
//! simplification, decorrelation of `Apply` into joins (ADR-0012),
//! predicate pushdown that moves only leakproof predicates below a
//! `PolicyFilter` (ADR-0018), projection pruning, and limit pushdown with
//! sort elimination based on derived output order. There is no cost model:
//! joins keep the order the query gave them.

mod api;
mod decorrelate;
mod order;
mod prune;
mod pushdown;
mod quantified;
mod simplify;
mod util;

pub use api::{is_leakproof, optimize};
pub(crate) use prune::{positional, referenced};
pub(crate) use util::{leakproof, max_binding, output_columns, produced, rewrite_expr, take};
