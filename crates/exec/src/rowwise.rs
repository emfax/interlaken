//! Row-at-a-time expression evaluation over Arrow batches.
//!
//! Until the vectorized kernels of milestone E2 replace it, every
//! expression is evaluated by the reference interpreter's row evaluator,
//! so the executor's semantics are the reference semantics by construction
//! (ADR-0023). E2's kernels are then tested against this path.

use arrow_array::{ArrayRef, BooleanArray, RecordBatch};
use interlaken_ir::interp::{Datum, RowEvaluator};
use interlaken_ir::{DataType, Expr};

use crate::columnar::{build_array, datum_at};
use crate::error::ExecError;
use crate::exec::ExecContext;
use crate::physical::Layout;

/// Return the rows of `batch` as values.
pub(crate) fn rows_of(batch: &RecordBatch) -> Result<Vec<Vec<Datum>>, ExecError> {
    (0..batch.num_rows())
        .map(|r| {
            batch
                .columns()
                .iter()
                .map(|c| datum_at(c.as_ref(), r))
                .collect()
        })
        .collect()
}

/// Evaluate `expr` on every row, building a column of type `ty`.
pub(crate) fn eval_column(
    ctx: &ExecContext,
    expr: &Expr,
    layout: &Layout,
    rows: &[Vec<Datum>],
    ty: &DataType,
) -> Result<ArrayRef, ExecError> {
    let mut evaluator = RowEvaluator::new(ctx.catalog.as_ref(), ctx.params.clone(), ctx.options);
    let values = rows
        .iter()
        .map(|row| evaluator.eval(expr, layout, row))
        .collect::<Result<Vec<_>, _>>()?;
    build_array(ty, &values)
}

/// Evaluate a predicate on every row: TRUE keeps a row, NULL and FALSE drop it.
pub(crate) fn eval_predicate(
    ctx: &ExecContext,
    expr: &Expr,
    layout: &Layout,
    rows: &[Vec<Datum>],
) -> Result<BooleanArray, ExecError> {
    let mut evaluator = RowEvaluator::new(ctx.catalog.as_ref(), ctx.params.clone(), ctx.options);
    rows.iter()
        .map(|row| {
            Ok(Some(matches!(
                evaluator.eval(expr, layout, row)?,
                Datum::Bool(true)
            )))
        })
        .collect()
}

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
