//! Running physical plans.

use std::sync::Arc;

use arrow_array::RecordBatch;
use futures::StreamExt;
use futures::stream::{self, BoxStream, TryStreamExt};
use interlaken_ir::catalog::Catalog;
use interlaken_ir::interp::{Datum, ExecOptions};
use interlaken_ir::{BindingId, ColumnId, Expr, Type};
use interlaken_storage::Txn;

use crate::error::ExecError;
use crate::kernels::CompiledExpr;
use crate::operators::{
    aggregate, apply, distinct, filter, join, limit, project, set_op, sort, table_scan, values,
};
use crate::physical::{Op, Physical};

/// A stream of result batches.
pub type BatchStream = BoxStream<'static, Result<RecordBatch, ExecError>>;

/// A column of an enclosing row, visible to a correlated subplan.
#[derive(Clone, Debug)]
pub struct OuterColumn {
    /// The column's binding.
    pub binding: BindingId,
    /// The column.
    pub column: ColumnId,
    /// Its type.
    pub ty: Type,
    /// Its value in the current outer row.
    pub value: Datum,
}

/// What a query runs with: its transaction, catalog and parameters.
#[derive(Clone)]
pub struct ExecContext {
    /// The transaction every leaf reads through (and DML writes into).
    pub txn: Txn,
    /// The catalog.
    pub catalog: Arc<dyn Catalog>,
    /// Parameter values, `$1` first.
    pub params: Vec<Datum>,
    /// Evaluation settings such as `now()`.
    pub options: ExecOptions,
    /// Columns of enclosing `Apply` outer rows, innermost first; empty at the top level.
    pub outer: Arc<[OuterColumn]>,
}

impl ExecContext {
    /// Construct a top-level context.
    #[must_use]
    pub fn new(
        txn: Txn,
        catalog: Arc<dyn Catalog>,
        params: Vec<Datum>,
        options: ExecOptions,
    ) -> Self {
        Self {
            txn,
            catalog,
            params,
            options,
            outer: Arc::new([]),
        }
    }

    /// Return a context for a subplan correlated with `row`, whose columns come first.
    #[must_use]
    pub fn correlated(&self, row: impl IntoIterator<Item = OuterColumn>) -> Self {
        let outer: Arc<[OuterColumn]> = row.into_iter().chain(self.outer.iter().cloned()).collect();
        Self {
            outer,
            ..self.clone()
        }
    }
}

/// Start executing `node`, returning its batches as a stream.
///
/// Execution is pull-based and lazy: nothing reads storage until the stream
/// is polled, and dropping the stream cancels the query.
#[must_use]
pub fn execute(node: &Physical, ctx: &ExecContext) -> BatchStream {
    match &node.op {
        Op::TableScan {
            table,
            pushed,
            batch_rows,
        } => table_scan(node, table.clone(), pushed.clone(), *batch_rows, ctx),
        Op::Filter { input, predicate } => match compile(predicate, input, ctx) {
            Ok(predicate) => filter(execute(input, ctx), predicate),
            Err(e) => failed(e),
        },
        Op::Project { input, exprs } => {
            match exprs.iter().map(|e| compile(e, input, ctx)).collect() {
                Ok(exprs) => project(execute(input, ctx), exprs, node.schema.clone()),
                Err(e) => failed(e),
            }
        }
        Op::Limit {
            input,
            limit: count,
            offset,
        } => limit(execute(input, ctx), count.clone(), offset.clone(), ctx),
        Op::Join {
            kind,
            left,
            right,
            keys,
            residual,
        } => join(node, *kind, left, right, keys, residual.as_ref(), ctx).unwrap_or_else(failed),
        Op::Apply { kind, outer, inner } => apply(node, *kind, outer, inner, ctx),
        Op::Aggregate { input, keys, aggs } => {
            aggregate(node, input, keys, aggs, ctx).unwrap_or_else(failed)
        }
        Op::Distinct { input } => distinct(execute(input, ctx), node.schema.clone()),
        Op::Sort { input, keys, fetch } => {
            sort(node, input, keys, fetch.as_ref(), ctx).unwrap_or_else(failed)
        }
        Op::SetOp {
            op,
            all,
            left,
            right,
        } => set_op(node, *op, *all, left, right, ctx),
        Op::Values { rows } => values(node, rows, ctx),
    }
}

/// Compile `expr` against the output columns of `input`.
fn compile(expr: &Expr, input: &Physical, ctx: &ExecContext) -> Result<CompiledExpr, ExecError> {
    compile_over(expr, &input.layout, &input.types, ctx)
}

/// Compile `expr` for inputs with columns `layout` of types `types`, in `ctx`'s scope.
pub(crate) fn compile_over(
    expr: &Expr,
    layout: &[(BindingId, ColumnId)],
    types: &[Type],
    ctx: &ExecContext,
) -> Result<CompiledExpr, ExecError> {
    let columns: Vec<_> = layout
        .iter()
        .zip(types)
        .map(|((b, c), ty)| (*b, *c, ty.clone()))
        .collect();
    CompiledExpr::compile(
        expr,
        &columns,
        &ctx.outer,
        ctx.catalog.as_ref(),
        &ctx.params,
        ctx.options,
    )
}

/// Return a stream that fails with `error` when first polled.
fn failed(error: ExecError) -> BatchStream {
    stream::once(async move { Err(error) }).boxed()
}

/// Execute `node` and collect every batch.
///
/// # Errors
///
/// Returns the first error the query raises.
pub async fn collect(node: &Physical, ctx: &ExecContext) -> Result<Vec<RecordBatch>, ExecError> {
    execute(node, ctx).try_collect().await
}
