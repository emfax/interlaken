//! The reference interpreter: a deliberately naive evaluator of resolved IR (ADR-0021).
//!
//! It defines what a plan means. The optimizer is tested by running
//! plans before and after optimization through it, and the executor's
//! operators are tested against it. It favours being obviously correct
//! over being fast: rows are vectors of [`Datum`]s, joins are nested
//! loops, and every subquery is re-evaluated per row.
//!
//! Semantics follow Postgres (ADR-0024): three-valued logic; integer
//! overflow, division by zero and failed casts are errors; `AND`, `OR`,
//! `CASE` and `COALESCE` evaluate operands left to right and stop as soon
//! as the result is known, so untaken branches never raise errors. DML
//! runs against an in-memory [`Database`] and is all-or-nothing per
//! statement. Known gaps: `jsonb` values are stored as unvalidated text,
//! and text conversion of dates, timestamps and floats follows Postgres's
//! output formats in UTC but only the ISO input formats.

mod cast;
mod datum;
mod error;
mod eval;
mod functions;
mod like;
mod rel;
mod run;

pub use cast::cast_value;
pub use datum::{Datum, RowKey};
pub use error::EvalError;
pub use functions::{REDACTED, aggregate_values, mask_last4};
pub use like::LikePattern;
pub use rel::compare_sort_keys;
pub(crate) use run::eval_constant;
pub use run::{Database, ExecOptions, RowEvaluator, execute};
