//! Vectorized expression evaluation with Postgres semantics (ADR-0024).
//!
//! An expression is compiled once per operator into a typed node tree
//! ([`CompiledExpr`]) and then evaluated over whole Arrow arrays, batch by
//! batch. Semantics are those of the reference interpreter, row for row:
//!
//! - NULL propagates through strict operators and functions, and `AND`,
//!   `OR` and `NOT` use three-valued logic.
//! - Integer overflow, division by zero and failed casts raise the
//!   interpreter's errors; nothing wraps or silently becomes NULL.
//! - `AND`, `OR`, `CASE` and `COALESCE` evaluate each operand only on the
//!   rows that reach it, so a branch a row does not take can never raise an
//!   error for it. The rows are compacted with `take` (keeping only the
//!   columns the operand reads), evaluated, and merged back.
//! - `Try` contains errors per row: a failing batch is bisected until the
//!   failing rows are isolated, and only those get the default.
//!
//! Each row therefore evaluates exactly the subexpressions it would in the
//! interpreter. A batch fails if and only if some row fails there, and with
//! an error that row raises.

mod arith;
mod cast;
mod compare;
mod compile;
mod eval;
mod functions;
mod input;
mod strings;

pub use compile::CompiledExpr;
