//! Running physical plans.

use std::sync::Arc;
use std::time::Instant;

use arrow_array::RecordBatch;
use futures::StreamExt;
use futures::stream::{self, BoxStream, TryStreamExt};
use interlaken_ir::catalog::{Catalog, TableDef};
use interlaken_ir::interp::{Datum, ExecOptions};
use interlaken_ir::{BindingId, ColumnId, Expr, Type};
use interlaken_storage::Txn;

use crate::error::ExecError;
use crate::kernels::CompiledExpr;
use crate::lookup::scan_below_filters;
use crate::memory::{DEFAULT_QUERY_BUDGET, QueryMemory, Reservation};
use crate::metrics::{Metrics, NodeMetrics, metered};
use crate::operators::{
    DmlKind, LookupSpec, aggregate, apply, distinct, dml, filter, index_lookup, join, limit, nest,
    project, set_op, sort, table_scan, unnest, values, write_check,
};
use crate::physical::{Op, Physical};
use crate::usage::UsageCounters;

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
    /// What the statement read from storage, for its usage (ADR-0039).
    pub usage: Arc<UsageCounters>,
    /// When the statement times out, if it has a timeout: every operator
    /// fails with [`ExecError::QueryCanceled`] at its next batch after it.
    pub deadline: Option<Instant>,
    /// The prepared cascade of each foreign key the statement's deletes
    /// can set off (ADR-0035); without one, a cascade that has rows to
    /// change fails.
    pub cascades: Option<Arc<crate::references::CascadePlans>>,
    /// The statement's foreign key checks, shared by its DML operators and
    /// the cascades they set off.
    pub(crate) constraints: Arc<crate::references::Constraints>,
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
            usage: Arc::default(),
            deadline: None,
            cascades: None,
            constraints: Arc::default(),
        }
    }

    /// Return this context with `cascades` for the foreign keys its deletes can set off.
    #[must_use]
    pub fn with_cascades(self, cascades: Arc<crate::references::CascadePlans>) -> Self {
        Self {
            cascades: Some(cascades),
            ..self
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

    /// Return this context counting what it reads into `usage`.
    #[must_use]
    pub fn with_usage(self, usage: Arc<UsageCounters>) -> Self {
        Self { usage, ..self }
    }

    /// Return this context cancelling the statement at `deadline`.
    #[must_use]
    pub fn with_deadline(self, deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
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
/// is polled, and dropping the stream cancels the query. With a deadline
/// (`statement_timeout`), the query fails once it passes.
#[must_use]
pub fn execute(node: &Physical, ctx: &ExecContext) -> BatchStream {
    let mut stream = start(node, ctx);
    if let Some(deadline) = ctx.deadline {
        // Checked per batch, so a statement stops on time even when its
        // operators never wait on storage (and so never yield).
        stream = stream
            .map(move |batch| {
                if Instant::now() >= deadline {
                    Err(ExecError::QueryCanceled)
                } else {
                    batch
                }
            })
            .boxed();
    }
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
        Op::IndexLookup {
            kind,
            left,
            right,
            key,
            keys,
            prefix,
            point,
            residual,
        } => match lookup_scan(right) {
            Some((table, batch_rows)) => index_lookup(
                node,
                &LookupSpec {
                    kind: *kind,
                    left,
                    right,
                    table,
                    key,
                    keys,
                    prefix: *prefix,
                    point: *point,
                    residual: residual.as_ref(),
                    batch_rows,
                },
                ctx,
            )
            .unwrap_or_else(failed),
            None => failed(ExecError::Unsupported(
                "index lookup without a table scan".into(),
            )),
        },
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
        Op::Values { rows, .. } => values(node, rows, ctx),
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

/// Return the table and batch size of the scan under an index lookup's right input.
fn lookup_scan(right: &Physical) -> Option<(&Arc<TableDef>, usize)> {
    match &scan_below_filters(right)?.op {
        Op::TableScan {
            table, batch_rows, ..
        } => Some((table, *batch_rows)),
        _ => None,
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
