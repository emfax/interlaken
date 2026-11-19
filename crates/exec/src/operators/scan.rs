//! `TableScan`: the leaf that reads a table through the transaction (ADR-0023).

use std::sync::Arc;

use arrow_array::RecordBatch;
use futures::stream::{self, StreamExt};
use interlaken_ir::Expr;
use interlaken_ir::catalog::{Catalog, TableDef};
use interlaken_ir::interp::{Datum, ExecOptions, RowEvaluator};
use interlaken_storage::catalog::TableDef as StoredTable;
use interlaken_storage::encoding::{ColumnType, Datum as Stored, decode_row};
use interlaken_storage::rows::{self, RowScan};
use interlaken_storage::{Key, RowFilter};

use crate::columnar::build_array;
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext};
use crate::physical::{Layout, Physical};

/// Convert a stored value to an interpreter value.
pub(crate) fn from_stored(d: &Stored) -> Datum {
    match d {
        Stored::Null => Datum::Null,
        Stored::Bool(v) => Datum::Bool(*v),
        Stored::Int2(v) => Datum::Int2(*v),
        Stored::Int4(v) => Datum::Int4(*v),
        Stored::Int8(v) => Datum::Int8(*v),
        Stored::Float8(v) => Datum::Float8(*v),
        Stored::Text(v) => Datum::Text(v.clone()),
        Stored::Bytea(v) => Datum::Bytea(v.clone()),
    }
}

/// A pushed-down predicate evaluated on the storage thread, row at a time,
/// with the reference evaluator (ADR-0023).
///
/// Only leakproof predicates are pushed, so evaluation cannot fail on any
/// data; a failure would be an engine bug and rejects the row.
struct PushedFilter {
    /// The predicate.
    predicate: Expr,
    /// The scan's output layout, which the predicate's references name.
    layout: Layout,
    /// The stored column types, for decoding.
    types: Vec<ColumnType>,
    /// The catalog, for function definitions.
    catalog: Arc<dyn Catalog>,
    /// Parameter values.
    params: Vec<Datum>,
    /// Evaluation settings.
    options: ExecOptions,
}

impl RowFilter for PushedFilter {
    /// Decode the row and keep it if the predicate is TRUE.
    fn eval(&self, _key: &[u8], value: &[u8]) -> bool {
        let Ok((_, row)) = decode_row(value, &self.types) else {
            return false;
        };
        let row: Vec<Datum> = row.iter().map(from_stored).collect();
        let mut evaluator =
            RowEvaluator::new(self.catalog.as_ref(), self.params.clone(), self.options);
        matches!(
            evaluator.eval(&self.predicate, &self.layout, &row),
            Ok(Datum::Bool(true))
        )
    }
}

/// Where a scan is.
struct State {
    /// The executor context.
    ctx: ExecContext,
    /// The IR table.
    table: Arc<TableDef>,
    /// The stored table, looked up on the first pull.
    stored: Option<StoredTable>,
    /// The storage filter, built on the first pull.
    filter: Option<Arc<dyn RowFilter>>,
    /// The pushed predicate.
    pushed: Option<Expr>,
    /// The scan's layout.
    layout: Layout,
    /// The output column types.
    types: Vec<interlaken_ir::Type>,
    /// The output schema.
    schema: arrow_schema::SchemaRef,
    /// Rows per storage call.
    batch_rows: usize,
    /// Where the next storage call resumes.
    resume: Option<Key>,
    /// Whether the last batch was read.
    done: bool,
}

impl State {
    /// Read storage batches until one has rows or the scan ends.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        loop {
            if self.done {
                return Ok(None);
            }
            if self.stored.is_none() {
                let name = self.table.name.last().to_string();
                let stored = interlaken_storage::catalog::table(&self.ctx.txn, &name)
                    .await?
                    .ok_or_else(|| ExecError::Catalog(format!("table {name} is not stored")))?;
                if let Some(predicate) = &self.pushed {
                    self.filter = Some(Arc::new(PushedFilter {
                        predicate: predicate.clone(),
                        layout: self.layout.clone(),
                        types: stored.types(),
                        catalog: self.ctx.catalog.clone(),
                        params: self.ctx.params.clone(),
                        options: self.ctx.options,
                    }));
                }
                self.stored = Some(stored);
            }
            let stored = self.stored.as_ref().expect("looked up above");
            let mut scan = RowScan::all();
            scan.limit_rows = self.batch_rows;
            scan.resume = self.resume.take();
            let batch = match &self.filter {
                Some(filter) => {
                    rows::scan_filtered(&self.ctx.txn, stored, &scan, filter.clone()).await?
                }
                None => rows::scan(&self.ctx.txn, stored, &scan).await?,
            };
            self.resume = batch.resume;
            self.done = self.resume.is_none();
            if batch.rows.is_empty() {
                continue;
            }
            let columns = (0..self.types.len())
                .map(|c| {
                    let values: Vec<Datum> = batch
                        .rows
                        .iter()
                        .map(|r| r.get(c).map_or(Datum::Null, from_stored))
                        .collect();
                    build_array(&self.types[c].data, &values)
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Some(RecordBatch::try_new(self.schema.clone(), columns)?));
        }
    }
}

/// Stream the rows of `node`, a table scan, in batches.
pub(crate) fn table_scan(
    node: &Physical,
    table: Arc<TableDef>,
    pushed: Option<Expr>,
    batch_rows: usize,
    ctx: &ExecContext,
) -> BatchStream {
    let state = State {
        ctx: ctx.clone(),
        table,
        stored: None,
        filter: None,
        pushed,
        layout: node.layout.clone(),
        types: node.types.clone(),
        schema: node.schema.clone(),
        batch_rows,
        resume: None,
        done: false,
    };
    stream::try_unfold(state, |mut state| async move {
        Ok(state.next().await?.map(|b| (b, state)))
    })
    .boxed()
}
