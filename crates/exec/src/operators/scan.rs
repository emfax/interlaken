//! `TableScan`: the leaf that reads a table through the transaction (ADR-0023).
//!
//! It reaches rows by its access path: every row, the rows under a
//! primary key prefix and range (one `scan` call per batch), or the rows
//! found through a secondary index (one `scan_index` call for a batch of
//! primary keys, then one `get_many` call for their rows). Key values are
//! evaluated once, when the scan starts; a NULL key value matches nothing.

use std::sync::Arc;

use arrow_array::RecordBatch;
use futures::stream::{self, StreamExt};
use interlaken_ir::Expr;
use interlaken_ir::catalog::{Catalog, TableDef};
use interlaken_ir::interp::{Datum, ExecOptions, RowEvaluator};
use interlaken_storage::RowFilter;
use interlaken_storage::catalog::IndexDef as StoredIndex;
use interlaken_storage::catalog::TableDef as StoredTable;
use interlaken_storage::encoding::{ColumnType, Datum as Stored, decode_row};
use interlaken_storage::rows::{self, RowScan, ValueRange};

use crate::access::{Access, KeyBounds};
use crate::columnar::{build_array, datum_at};
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over};
use crate::metrics::NodeMetrics;
use crate::physical::{Layout, Physical};

/// Convert an interpreter value to its stored form, if it has one.
///
/// Scalars convert to their storage namesakes and arrays of scalars to
/// storage arrays; records and nested arrays have no stored form.
#[must_use]
pub fn to_stored(d: &Datum) -> Option<Stored> {
    Some(match d {
        Datum::Null => Stored::Null,
        Datum::Bool(v) => Stored::Bool(*v),
        Datum::Int2(v) => Stored::Int2(*v),
        Datum::Int4(v) => Stored::Int4(*v),
        Datum::Int8(v) => Stored::Int8(*v),
        Datum::Float8(v) => Stored::Float8(*v),
        Datum::Text(v) => Stored::Text(v.clone()),
        Datum::Bytea(v) => Stored::Bytea(v.clone()),
        Datum::Date(v) => Stored::Date(*v),
        Datum::Timestamptz(v) => Stored::Timestamptz(*v),
        Datum::Uuid(v) => Stored::Uuid(*v),
        Datum::Jsonb(v) => Stored::Jsonb(v.clone()),
        Datum::Array(items) => Stored::Array(
            items
                .iter()
                .map(|item| match item {
                    Datum::Array(_) => None,
                    scalar => to_stored(scalar),
                })
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

/// Convert a stored value to an interpreter value.
#[must_use]
pub fn from_stored(d: &Stored) -> Datum {
    match d {
        Stored::Null => Datum::Null,
        Stored::Bool(v) => Datum::Bool(*v),
        Stored::Int2(v) => Datum::Int2(*v),
        Stored::Int4(v) => Datum::Int4(*v),
        Stored::Int8(v) => Datum::Int8(*v),
        Stored::Float8(v) => Datum::Float8(*v),
        Stored::Text(v) => Datum::Text(v.clone()),
        Stored::Bytea(v) => Datum::Bytea(v.clone()),
        Stored::Date(v) => Datum::Date(*v),
        Stored::Timestamptz(v) => Datum::Timestamptz(*v),
        Stored::Uuid(v) => Datum::Uuid(*v),
        Stored::Jsonb(v) => Datum::Jsonb(v.clone()),
        Stored::Array(items) => Datum::Array(items.iter().map(from_stored).collect()),
    }
}

/// Return the storage column type that holds values of an IR type, if any.
///
/// Every scalar maps to its storage namesake, and arrays of scalars to
/// storage arrays. Records, nested arrays and entity references have no
/// stored form.
#[must_use]
pub fn stored_type(ty: &interlaken_ir::DataType) -> Option<ColumnType> {
    use interlaken_ir::{DataType, ScalarType};
    use interlaken_storage::encoding::ElementType;
    Some(match ty {
        DataType::Scalar(s) => match s {
            ScalarType::Bool => ColumnType::Bool,
            ScalarType::Int2 => ColumnType::Int2,
            ScalarType::Int4 => ColumnType::Int4,
            ScalarType::Int8 => ColumnType::Int8,
            ScalarType::Float8 => ColumnType::Float8,
            ScalarType::Text => ColumnType::Text,
            ScalarType::Bytea => ColumnType::Bytea,
            ScalarType::Date => ColumnType::Date,
            ScalarType::Timestamptz => ColumnType::Timestamptz,
            ScalarType::Uuid => ColumnType::Uuid,
            ScalarType::Jsonb => ColumnType::Jsonb,
        },
        DataType::Array(element) => {
            ColumnType::Array(ElementType::of(stored_type(&element.data)?)?)
        }
        _ => return None,
    })
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

/// The storage-side form of an access path, resolved when the scan starts.
enum Resolved {
    /// Nothing can match (a key value is NULL).
    Empty,
    /// A primary key scan.
    Primary(RowScan),
    /// A secondary index scan.
    Index(StoredIndex, RowScan),
}

/// Where a scan is.
struct State {
    /// The executor context.
    ctx: ExecContext,
    /// The IR table.
    table: Arc<TableDef>,
    /// The access path.
    access: Access,
    /// The stored table, looked up on the first pull.
    stored: Option<StoredTable>,
    /// The resolved access path, built on the first pull.
    resolved: Option<Resolved>,
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
    /// Whether the last batch was read.
    done: bool,
    /// Where to count storage calls, for `EXPLAIN ANALYZE`.
    metrics: Option<Arc<NodeMetrics>>,
}

/// Return the bytes a stored value occupies when decoded (an estimate for `EXPLAIN ANALYZE`).
fn stored_bytes(d: &Stored) -> usize {
    match d {
        Stored::Null => 0,
        Stored::Bool(_) => 1,
        Stored::Int2(_) => 2,
        Stored::Int4(_) | Stored::Date(_) => 4,
        Stored::Int8(_) | Stored::Float8(_) | Stored::Timestamptz(_) => 8,
        Stored::Uuid(_) => 16,
        Stored::Text(s) | Stored::Jsonb(s) => s.len(),
        Stored::Bytea(b) => b.len(),
        Stored::Array(items) => items.iter().map(stored_bytes).sum(),
    }
}

/// Return the bytes of the values in `rows`.
fn rows_bytes(rows: &[Vec<Stored>]) -> usize {
    rows.iter().flatten().map(stored_bytes).sum()
}

/// Evaluate a key value (an expression reading no columns of the scanned table).
fn key_value(expr: &Expr, ctx: &ExecContext) -> Result<Datum, ExecError> {
    let value = compile_over(expr, &[], &[], ctx)?.eval(&[], 1)?;
    datum_at(value.as_ref(), 0)
}

/// Return the stored form of a key value, or `None` if it is NULL.
fn stored_key(expr: &Expr, ctx: &ExecContext) -> Result<Option<Stored>, ExecError> {
    match key_value(expr, ctx)? {
        Datum::Null => Ok(None),
        value => to_stored(&value)
            .map(Some)
            .ok_or_else(|| ExecError::Unsupported(format!("key value {value:?}"))),
    }
}

/// Resolve key bounds into a storage scan, or `None` if a key value is NULL.
fn resolve_bounds(
    bounds: &KeyBounds,
    batch_rows: usize,
    ctx: &ExecContext,
) -> Result<Option<RowScan>, ExecError> {
    let mut prefix = Vec::with_capacity(bounds.prefix.len());
    for e in &bounds.prefix {
        match stored_key(e, ctx)? {
            Some(v) => prefix.push(v),
            None => return Ok(None),
        }
    }
    let bound = |b: &std::ops::Bound<Expr>| -> Result<Option<std::ops::Bound<Stored>>, ExecError> {
        use std::ops::Bound;
        Ok(match b {
            Bound::Unbounded => Some(Bound::Unbounded),
            Bound::Included(e) => stored_key(e, ctx)?.map(Bound::Included),
            Bound::Excluded(e) => stored_key(e, ctx)?.map(Bound::Excluded),
        })
    };
    let (Some(lower), Some(upper)) = (bound(&bounds.lower)?, bound(&bounds.upper)?) else {
        return Ok(None);
    };
    let mut scan = RowScan::prefix(prefix);
    scan.limit_rows = batch_rows;
    if !matches!(
        (&lower, &upper),
        (std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
    ) {
        scan.range = Some(ValueRange { lower, upper });
    }
    Ok(Some(scan))
}

impl State {
    /// Look up the stored table, build the storage filter and resolve the access path.
    async fn start(&mut self) -> Result<(), ExecError> {
        let name = self.table.name.joined();
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
        let resolved = match &self.access {
            Access::Full => {
                let mut scan = RowScan::all();
                scan.limit_rows = self.batch_rows;
                Resolved::Primary(scan)
            }
            Access::Primary(bounds) => match resolve_bounds(bounds, self.batch_rows, &self.ctx)? {
                Some(scan) => Resolved::Primary(scan),
                None => Resolved::Empty,
            },
            Access::Index { name, bounds } => {
                let index = stored
                    .index(name.as_str())
                    .cloned()
                    .ok_or_else(|| ExecError::Catalog(format!("index {name} is not stored")))?;
                match resolve_bounds(bounds, self.batch_rows, &self.ctx)? {
                    Some(scan) => Resolved::Index(index, scan),
                    None => Resolved::Empty,
                }
            }
        };
        self.stored = Some(stored);
        self.resolved = Some(resolved);
        Ok(())
    }

    /// Read the next batch of stored rows, or `None` when the scan is complete.
    async fn read(&mut self) -> Result<Option<Vec<Vec<Stored>>>, ExecError> {
        let stored = self.stored.as_ref().expect("started");
        match self.resolved.as_mut().expect("started") {
            Resolved::Empty => Ok(None),
            Resolved::Primary(scan) => {
                let batch = match &self.filter {
                    Some(filter) => {
                        rows::scan_filtered(&self.ctx.txn, stored, scan, filter.clone()).await?
                    }
                    None => rows::scan(&self.ctx.txn, stored, scan).await?,
                };
                let bytes = rows_bytes(&batch.rows);
                self.ctx.usage.record_read(batch.rows.len(), bytes);
                if let Some(m) = &self.metrics {
                    m.record_storage_call(bytes);
                }
                self.done = batch.resume.is_none();
                scan.resume = batch.resume;
                Ok(Some(batch.rows))
            }
            Resolved::Index(index, scan) => {
                let batch = rows::scan_index(&self.ctx.txn, stored, index, scan).await?;
                self.done = batch.resume.is_none();
                scan.resume = batch.resume;
                let found: Vec<Vec<Stored>> =
                    rows::get_many(&self.ctx.txn, stored, &batch.primary_keys)
                        .await?
                        .into_iter()
                        .flatten()
                        .collect();
                let (key_bytes, row_bytes) = (rows_bytes(&batch.primary_keys), rows_bytes(&found));
                self.ctx
                    .usage
                    .record_read(found.len(), key_bytes + row_bytes);
                if let Some(m) = &self.metrics {
                    m.record_storage_call(key_bytes);
                    m.record_storage_call(row_bytes);
                }
                Ok(Some(found))
            }
        }
    }

    /// Read storage batches until one has rows or the scan ends.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if self.resolved.is_none() {
            self.start().await?;
        }
        loop {
            if self.done {
                return Ok(None);
            }
            let Some(rows) = self.read().await? else {
                return Ok(None);
            };
            if rows.is_empty() {
                continue;
            }
            let columns = (0..self.types.len())
                .map(|c| {
                    let values: Vec<Datum> = rows
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
    access: Access,
    pushed: Option<Expr>,
    batch_rows: usize,
    ctx: &ExecContext,
) -> BatchStream {
    let state = State {
        ctx: ctx.clone(),
        table,
        access,
        stored: None,
        resolved: None,
        filter: None,
        pushed,
        layout: node.layout.clone(),
        types: node.types.clone(),
        schema: node.schema.clone(),
        batch_rows,
        done: false,
        metrics: ctx.node_metrics(node.id),
    };
    stream::try_unfold(state, |mut state| async move {
        Ok(state.next().await?.map(|b| (b, state)))
    })
    .boxed()
}
