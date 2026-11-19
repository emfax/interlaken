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
use crate::memory::{DEFAULT_QUERY_BUDGET, QueryMemory, Reservation};
use crate::metrics::{Metrics, NodeMetrics, metered};
use crate::operators::{
    DmlKind, aggregate, apply, distinct, dml, filter, join, limit, nest, project, set_op, sort,
    table_scan, unnest, values, write_check,
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

/// What a query runs with: its transaction, catalog, parameters, memory
/// budget and (for `EXPLAIN ANALYZE`) metrics.
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
    /// The query's memory budget, shared by all its operators (ADR-0027).
    pub memory: Arc<QueryMemory>,
    /// Per-operator metrics, collected when set.
    pub metrics: Option<Arc<Metrics>>,
}

impl ExecContext {
    /// Construct a top-level context with the default memory budget and no metrics.
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
            memory: QueryMemory::new(DEFAULT_QUERY_BUDGET, None),
            metrics: None,
        }
    }

    /// Return this context with the memory budget `memory`.
    #[must_use]
    pub fn with_memory(self, memory: Arc<QueryMemory>) -> Self {
        Self { memory, ..self }
    }

    /// Return this context collecting per-operator metrics into `metrics`.
    #[must_use]
    pub fn with_metrics(self, metrics: Arc<Metrics>) -> Self {
        Self {
            metrics: Some(metrics),
            ..self
        }
    }

    /// Return the metrics of operator `id`, if metrics are collected.
    pub(crate) fn node_metrics(&self, id: usize) -> Option<Arc<NodeMetrics>> {
        self.metrics.as_ref()?.node(id).cloned()
    }

    /// Start an empty memory reservation for `node`, labelled `operator` in errors.
    pub(crate) fn reserve(&self, node: &Physical, operator: &'static str) -> Reservation {
        Reservation::new(self.memory.clone(), operator, self.node_metrics(node.id))
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
    let stream = start(node, ctx);
    match ctx.node_metrics(node.id) {
        Some(metrics) => metered(stream, metrics),
        None => stream,
    }
}

/// Build the stream of one operator.
fn start(node: &Physical, ctx: &ExecContext) -> BatchStream {
    match &node.op {
        Op::TableScan {
            table,
            access,
            pushed,
            policy,
            batch_rows,
            ..
        } => {
            // Storage evaluates user and policy conjuncts alike; both are leakproof.
            let predicate = match (pushed.clone(), policy.clone()) {
                (Some(a), Some(b)) => Some(Expr::And(vec![b, a])),
                (a, b) => a.or(b),
            };
            table_scan(
                node,
                table.clone(),
                access.clone(),
                predicate,
                *batch_rows,
                ctx,
            )
        }
        Op::Filter { input, predicate }
        | Op::PolicyFilter {
            input,
            residual: predicate,
            ..
        } => match compile(predicate, input, ctx) {
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
        Op::Distinct { input } => distinct(
            execute(input, ctx),
            node.schema.clone(),
            ctx.reserve(node, "Distinct"),
        ),
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
        Op::Unnest { input, expr, width } => match compile(expr, input, ctx) {
            Ok(expr) => unnest(execute(input, ctx), expr, *width, node.schema.clone()),
            Err(e) => failed(e),
        },
        Op::Nest {
            input,
            keys,
            fields,
        } => nest(node, input, keys, fields, ctx).unwrap_or_else(failed),
        Op::WriteCheck {
            input,
            check,
            table,
        } => match compile(check, input, ctx) {
            Ok(check) => write_check(execute(input, ctx), check, table.clone()),
            Err(e) => failed(e),
        },
        Op::Insert { dml: d, columns } => {
            dml(node, d, DmlKind::Insert(columns), ctx).unwrap_or_else(failed)
        }
        Op::Update {
            dml: d,
            key,
            assignments,
        } => dml(node, d, DmlKind::Update(key, assignments), ctx).unwrap_or_else(failed),
        Op::Delete { dml: d, key } => {
            dml(node, d, DmlKind::Delete(key), ctx).unwrap_or_else(failed)
        }
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
