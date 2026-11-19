//! Running physical plans.

use std::sync::Arc;

use arrow_array::RecordBatch;
use futures::stream::{BoxStream, TryStreamExt};
use interlaken_ir::catalog::Catalog;
use interlaken_ir::interp::{Datum, ExecOptions};
use interlaken_storage::Txn;

use crate::error::ExecError;
use crate::operators::{filter, limit, project, table_scan};
use crate::physical::{Op, Physical};

/// A stream of result batches.
pub type BatchStream = BoxStream<'static, Result<RecordBatch, ExecError>>;

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
        Op::Filter { input, predicate } => filter(
            execute(input, ctx),
            predicate.clone(),
            input.layout.clone(),
            ctx,
        ),
        Op::Project { input, exprs } => project(
            execute(input, ctx),
            exprs.clone(),
            input.layout.clone(),
            node,
            ctx,
        ),
        Op::Limit {
            input,
            limit: count,
            offset,
        } => limit(execute(input, ctx), count.clone(), offset.clone(), ctx),
    }
}

/// Execute `node` and collect every batch.
///
/// # Errors
///
/// Returns the first error the query raises.
pub async fn collect(node: &Physical, ctx: &ExecContext) -> Result<Vec<RecordBatch>, ExecError> {
    execute(node, ctx).try_collect().await
}
