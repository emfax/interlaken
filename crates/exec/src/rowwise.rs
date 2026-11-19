//! Row-at-a-time evaluation with the reference interpreter.
//!
//! Used for expressions that read no columns (limits and offsets); filters
//! pushed into storage call the interpreter's row evaluator directly.

use interlaken_ir::Expr;
use interlaken_ir::interp::{Datum, RowEvaluator};

use crate::error::ExecError;
use crate::exec::ExecContext;

/// Evaluate an expression that reads no columns (a limit or offset).
pub(crate) fn eval_constant(ctx: &ExecContext, expr: &Expr) -> Result<Datum, ExecError> {
    Ok(
        RowEvaluator::new(ctx.catalog.as_ref(), ctx.params.clone(), ctx.options).eval(
            expr,
            &[],
            &[],
        )?,
    )
}
