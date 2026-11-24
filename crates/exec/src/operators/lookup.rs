//! `IndexLookup`: an index nested-loop join probing a whole batch of keys at once.
//!
//! For each left batch, the join keys that pin the right table's key are
//! evaluated, and the right rows with those keys are fetched in one storage
//! call per batch: `get_many` when the keys pin a whole unique primary key,
//! otherwise one prefix scan per distinct key, all in one `scan_many` call
//! (with one more `get_many` for a secondary index's rows). Prefixes with
//! more rows than one call returns are continued in further calls.
//!
//! The fetched rows then go through everything the right input would have
//! applied: the scan's pushed and policy conjuncts and each filter and
//! policy filter above it, in order from the scan up, so policy filters
//! still run before anything the join evaluates. From there the join works
//! like a hash join over just those rows: every equality key (including
//! any beyond the pinned prefix) must match, NULL keys never match, and
//! the residual runs on key-matching pairs (`join::pair_up`).
//!
//! Lookups pay off when the left input is small next to the right table,
//! which the planner can't know without statistics. So the operator first
//! reads both inputs, the right one faster: if the left input ends first
//! (within [`LOOKUP_ROWS`] rows), it looks those rows up; otherwise it runs
//! an ordinary hash join, building the right input as planned and probing
//! with every left row, including the ones read so far.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use arrow_array::{Array, ArrayRef, BooleanArray, RecordBatch};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use futures::stream;
use interlaken_ir::catalog::TableDef;
use interlaken_ir::interp::{Datum, RowKey};
use interlaken_ir::{Expr, JoinKind, Type};
use interlaken_storage::Txn;
use interlaken_storage::catalog::{IndexDef as StoredIndex, TableDef as StoredTable};
use interlaken_storage::encoding::Datum as Stored;
use interlaken_storage::rows::{self, RowScan};

use super::join::{JoinCondition, Pairing, hash_join, pair_up};
use super::pull::{Pull, into_stream, row_keys};
use super::scan::{from_stored, to_stored};
use crate::columnar::build_array;
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over, execute};
use crate::kernels::CompiledExpr;
use crate::lookup::LookupKey;
use crate::memory::{Reservation, batch_bytes};
use crate::metrics::NodeMetrics;
use crate::physical::{Op, Physical};
use crate::usage::UsageCounters;

/// The most keys (or prefix scans) in one storage call; a batch's keys go
/// in several concurrent calls, so the storage pool reads them in parallel.
const LOOKUP_CHUNK: usize = 256;

/// Right rows an index lookup join reads per left row while deciding: lookups
/// cost about this many times as much per left row as building a hash
/// table costs per right row (measured by the executor benchmarks).
const RIGHT_PER_LEFT: usize = 2;

/// The most left rows an index lookup join looks up; past this many it
/// becomes a hash join.
const LOOKUP_ROWS: usize = 128 * 1024;

/// Whether an index lookup join looks rows up or hashes.
enum Mode {
    /// Reading the first left rows to decide.
    Undecided,
    /// Looking up each left batch.
    Lookup,
    /// Running as a hash join.
    Hash(BatchStream),
}

/// What an index lookup join needs to become a hash join.
struct Fallback {
    /// The join's node.
    node: Physical,
    /// The left input.
    left: Physical,
    /// The right input.
    right: Physical,
    /// The equality keys.
    keys: Vec<(Expr, Expr)>,
    /// The residual condition.
    residual: Option<Expr>,
}

/// An index lookup join in progress.
struct LookupState {
    /// Lookup or hash join.
    mode: Mode,
    /// Left batches read while deciding, not yet joined.
    pending: VecDeque<RecordBatch>,
    /// Whether the left input has ended.
    left_done: bool,
    /// The most left rows to look up.
    lookup_rows: usize,
    /// How to become a hash join.
    fallback: Fallback,
    /// The join kind: inner, left, semi or anti.
    kind: JoinKind,
    /// The left input.
    left: BatchStream,
    /// The context, for the transaction.
    ctx: ExecContext,
    /// The right table.
    table: Arc<TableDef>,
    /// The key the right rows are reached by.
    key: LookupKey,
    /// How many leading key columns the left keys pin.
    prefix: usize,
    /// Whether the pinned columns are a whole unique primary key.
    point: bool,
    /// Where the right rows are read, set up on the first pull.
    source: Option<Arc<Source>>,
    /// Left key expressions, pinned columns first.
    left_keys: Vec<CompiledExpr>,
    /// Right key expressions, in the same order.
    right_keys: Vec<CompiledExpr>,
    /// The right input's conditions, from the scan up.
    stages: Vec<CompiledExpr>,
    /// The residual condition over left then right columns.
    residual: Option<CompiledExpr>,
    /// The right rows' column types.
    right_types: Vec<Type>,
    /// The right rows' schema.
    right_schema: SchemaRef,
    /// The output schema.
    schema: SchemaRef,
    /// Rows per storage scan batch.
    batch_rows: usize,
    /// The memory held by the fetched rows of the current batch.
    memory: Reservation,
    /// Where to count storage calls.
    metrics: Option<Arc<NodeMetrics>>,
}

/// Return the rows of a predicate result that are TRUE, as a mask.
fn true_mask(result: &ArrayRef) -> Result<BooleanArray, ExecError> {
    let result = result
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| ExecError::Unsupported("non-boolean condition".into()))?;
    Ok((0..result.len())
        .map(|i| Some(result.is_valid(i) && result.value(i)))
        .collect())
}

/// Convert key values to their stored form.
fn stored_values(values: &[Datum]) -> Result<Vec<Stored>, ExecError> {
    values
        .iter()
        .map(|v| to_stored(v).ok_or_else(|| ExecError::Unsupported(format!("key value {v:?}"))))
        .collect()
}

/// Where an index lookup reads the right table's rows.
struct Source {
    /// The transaction.
    txn: Txn,
    /// The stored table.
    stored: StoredTable,
    /// The stored index, for a secondary index.
    index: Option<StoredIndex>,
    /// Whether the pinned columns are a whole primary key.
    point: bool,
    /// Rows per storage scan batch.
    batch_rows: usize,
    /// Where to count storage calls.
    metrics: Option<Arc<NodeMetrics>>,
    /// Where to count the statement's reads.
    usage: Arc<UsageCounters>,
}

impl Source {
    /// Record one storage call that read `values`: rows if `rows`, or
    /// index entries' primary keys.
    fn record_call<'v>(&self, values: impl IntoIterator<Item = &'v Vec<Stored>>, rows: bool) {
        let (mut count, mut bytes) = (0, 0);
        for value in values {
            count += 1;
            bytes += value.iter().map(stored_len).sum::<usize>();
        }
        self.usage.record_read(if rows { count } else { 0 }, bytes);
        if let Some(m) = &self.metrics {
            m.record_storage_call(bytes);
        }
    }

    /// Read the rows with primary keys `keys`, in concurrent calls of at most
    /// [`LOOKUP_CHUNK`] keys, skipping keys with no row.
    async fn get_rows(&self, keys: &[Vec<Stored>]) -> Result<Vec<Vec<Stored>>, ExecError> {
        let calls = keys
            .chunks(LOOKUP_CHUNK)
            .map(|keys| rows::get_many(&self.txn, &self.stored, keys));
        let mut found = Vec::with_capacity(keys.len());
        for rows in futures::future::try_join_all(calls).await? {
            let rows: Vec<Vec<Stored>> = rows.into_iter().flatten().collect();
            self.record_call(&rows, true);
            found.extend(rows);
        }
        Ok(found)
    }

    /// Read one batch of each prefix scan (of the table or the index), in
    /// concurrent calls of at most [`LOOKUP_CHUNK`] scans; return the rows
    /// (or index entries' primary keys) and the scans left to continue.
    async fn scan_prefixes(
        &self,
        scans: Vec<RowScan>,
    ) -> Result<(Vec<Vec<Stored>>, Vec<RowScan>), ExecError> {
        let (txn, stored) = (&self.txn, &self.stored);
        let mut found = Vec::new();
        let mut resumed = Vec::new();
        let mut record = |mut scan: RowScan, values: Vec<Vec<Stored>>, resume| {
            found.extend(values);
            if let Some(resume) = resume {
                scan.resume = Some(resume);
                resumed.push(scan);
            }
        };
        match &self.index {
            None => {
                let calls = scans
                    .chunks(LOOKUP_CHUNK)
                    .map(|chunk| rows::scan_many(txn, stored, chunk));
                let mut batches = Vec::with_capacity(scans.len());
                for chunk in futures::future::try_join_all(calls).await? {
                    self.record_call(chunk.iter().flat_map(|b| &b.rows), true);
                    batches.extend(chunk);
                }
                for (scan, batch) in scans.into_iter().zip(batches) {
                    record(scan, batch.rows, batch.resume);
                }
            }
            Some(index) => {
                let calls = scans
                    .chunks(LOOKUP_CHUNK)
                    .map(|chunk| rows::scan_index_many(txn, stored, index, chunk));
                let mut batches = Vec::with_capacity(scans.len());
                for chunk in futures::future::try_join_all(calls).await? {
                    self.record_call(chunk.iter().flat_map(|b| &b.primary_keys), false);
                    batches.extend(chunk);
                }
                for (scan, batch) in scans.into_iter().zip(batches) {
                    record(scan, batch.primary_keys, batch.resume);
                }
            }
        }
        Ok((found, resumed))
    }

    /// Fetch every right row whose pinned key columns equal one of `prefixes`.
    async fn fetch(&self, prefixes: &[Vec<Stored>]) -> Result<Vec<Vec<Stored>>, ExecError> {
        if prefixes.is_empty() {
            return Ok(Vec::new());
        }
        if self.point {
            return self.get_rows(prefixes).await;
        }
        // One prefix scan per distinct key; scans with more rows continue in later calls.
        let mut scans: Vec<RowScan> = prefixes
            .iter()
            .map(|p| {
                let mut scan = RowScan::prefix(p.clone());
                scan.limit_rows = self.batch_rows;
                scan
            })
            .collect();
        let mut found: Vec<Vec<Stored>> = Vec::new();
        while !scans.is_empty() {
            let (values, resumed) = self.scan_prefixes(scans).await?;
            if self.index.is_some() {
                found.extend(self.get_rows(&values).await?);
            } else {
                found.extend(values);
            }
            scans = resumed;
        }
        Ok(found)
    }
}

impl LookupState {
    /// Look up the stored table (and index) on the first pull.
    async fn start(&mut self) -> Result<(), ExecError> {
        let name = self.table.name.last().to_string();
        let stored = interlaken_storage::catalog::table(&self.ctx.txn, &name)
            .await?
            .ok_or_else(|| ExecError::Catalog(format!("table {name} is not stored")))?;
        let index = match &self.key {
            LookupKey::Index(index) => Some(
                stored
                    .index(index.as_str())
                    .cloned()
                    .ok_or_else(|| ExecError::Catalog(format!("index {index} is not stored")))?,
            ),
            LookupKey::Primary => None,
        };
        self.source = Some(Arc::new(Source {
            txn: self.ctx.txn.clone(),
            stored,
            index,
            point: self.point,
            batch_rows: self.batch_rows,
            metrics: self.metrics.clone(),
            usage: self.ctx.usage.clone(),
        }));
        Ok(())
    }

    /// Decode fetched rows into a batch with the right input's schema.
    fn decode(&self, rows: &[Vec<Stored>]) -> Result<RecordBatch, ExecError> {
        let columns = self
            .right_types
            .iter()
            .enumerate()
            .map(|(c, ty)| {
                let values: Vec<Datum> = rows
                    .iter()
                    .map(|r| r.get(c).map_or(Datum::Null, from_stored))
                    .collect();
                build_array(&ty.data, &values)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RecordBatch::try_new(self.right_schema.clone(), columns)?)
    }

    /// Keep the fetched rows the right input's conditions accept, in order from the scan up.
    fn filter(&mut self, mut right: RecordBatch) -> Result<RecordBatch, ExecError> {
        for stage in &mut self.stages {
            if right.num_rows() == 0 {
                break;
            }
            let mask = true_mask(&stage.eval(right.columns(), right.num_rows())?)?;
            right = arrow_select::filter::filter_record_batch(&right, &mask)?;
        }
        Ok(right)
    }

    /// Join one left batch with the right rows its keys reach.
    async fn probe(&mut self, left: &RecordBatch) -> Result<RecordBatch, ExecError> {
        let n = left.num_rows();
        let keys = self
            .left_keys
            .iter_mut()
            .map(|k| k.eval(left.columns(), n))
            .collect::<Result<Vec<_>, _>>()?;
        let left_keys = row_keys(&keys, n)?;
        // Distinct pinned prefixes of rows whose keys have no NULL.
        let mut seen: HashSet<RowKey> = HashSet::new();
        let mut prefixes = Vec::new();
        for key in &left_keys {
            if key.0.iter().any(Datum::is_null) {
                continue;
            }
            let prefix = &key.0[..self.prefix];
            if seen.insert(RowKey(prefix.to_vec())) {
                prefixes.push(stored_values(prefix)?);
            }
        }
        let source = self.source.clone().expect("started");
        let fetched = source.fetch(&prefixes).await?;
        let right = self.decode(&fetched)?;
        drop(fetched);
        let right = self.filter(right)?;
        let held: usize = self.pending.iter().map(batch_bytes).sum();
        self.memory.resize(held + batch_bytes(&right))?;
        let right_key_values = self
            .right_keys
            .iter_mut()
            .map(|k| k.eval(right.columns(), right.num_rows()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut index: HashMap<RowKey, Vec<u32>> = HashMap::new();
        for (j, key) in row_keys(&right_key_values, right.num_rows())?
            .into_iter()
            .enumerate()
        {
            if !key.0.iter().any(Datum::is_null) {
                index
                    .entry(key)
                    .or_default()
                    .push(u32::try_from(j).expect("fetched rows fit in u32"));
            }
        }
        // The index holds no key with a NULL, so a left key with one finds nothing.
        let candidates: Vec<Vec<u32>> = left_keys
            .iter()
            .map(|key| index.get(key).cloned().unwrap_or_default())
            .collect();
        pair_up(
            &mut Pairing {
                kind: self.kind,
                residual: self.residual.as_mut(),
                schema: &self.schema,
                right: &right,
                right_matched: None,
            },
            left,
            &candidates,
        )
    }
}

/// Return the bytes a stored value occupies when decoded (an estimate for `EXPLAIN ANALYZE`).
fn stored_len(d: &Stored) -> usize {
    match d {
        Stored::Null => 0,
        Stored::Bool(_) => 1,
        Stored::Int2(_) => 2,
        Stored::Int4(_) | Stored::Date(_) => 4,
        Stored::Int8(_) | Stored::Float8(_) | Stored::Timestamptz(_) => 8,
        Stored::Uuid(_) => 16,
        Stored::Text(s) | Stored::Jsonb(s) => s.len(),
        Stored::Bytea(b) => b.len(),
        Stored::Array(items) => items.iter().map(stored_len).sum(),
    }
}

impl LookupState {
    /// Read both inputs, the right [`RIGHT_PER_LEFT`] times as fast, until one ends or both
    /// pass the limit; then look the left rows up, or run a hash join.
    ///
    /// If the left input ends first, the right table holds more than
    /// [`RIGHT_PER_LEFT`] rows per left row, and lookups cost less than
    /// building it. If the right input ends first, it is small enough to
    /// hash. If both pass the limit, lookups would be too many. Rows read
    /// on the way are kept and joined either way.
    async fn decide(&mut self) -> Result<(), ExecError> {
        let mut right = execute(&self.fallback.right, &self.ctx);
        let mut right_read: Vec<Result<RecordBatch, ExecError>> = Vec::new();
        let (mut left_rows, mut right_rows) = (0, 0);
        let mut right_done = false;
        loop {
            if !right_done && left_rows * RIGHT_PER_LEFT > right_rows {
                match right.next().await {
                    Some(b) => {
                        let b = b?;
                        right_rows += b.num_rows();
                        self.memory.grow(batch_bytes(&b))?;
                        right_read.push(Ok(b));
                    }
                    None => right_done = true,
                }
            } else {
                match self.left.next().await {
                    Some(b) => {
                        let b = b?;
                        left_rows += b.num_rows();
                        self.memory.grow(batch_bytes(&b))?;
                        self.pending.push_back(b);
                    }
                    None => {
                        // The left input is the small one: look it up.
                        self.left_done = true;
                        self.mode = Mode::Lookup;
                        self.memory
                            .resize(self.pending.iter().map(batch_bytes).sum())?;
                        return Ok(());
                    }
                }
            }
            if right_done || left_rows > self.lookup_rows {
                break;
            }
        }
        let read: Vec<Result<RecordBatch, ExecError>> = self.pending.drain(..).map(Ok).collect();
        let rest = std::mem::replace(&mut self.left, stream::empty().boxed());
        let left_batches = stream::iter(read).chain(rest).boxed();
        let right_rest = if right_done {
            stream::empty().boxed()
        } else {
            right
        };
        let right_batches = stream::iter(right_read).chain(right_rest).boxed();
        let f = &self.fallback;
        let hashed = hash_join(
            &f.node,
            self.kind,
            (left_batches, right_batches),
            &f.left,
            &f.right,
            &JoinCondition {
                keys: &f.keys,
                residual: f.residual.as_ref(),
            },
            &self.ctx,
        )?;
        self.memory.resize(0)?;
        self.mode = Mode::Hash(hashed);
        Ok(())
    }
}

impl Pull for LookupState {
    /// Decide on the first pull, then probe (or hash-join) one left batch per pull.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if matches!(self.mode, Mode::Undecided) {
            self.decide().await?;
        }
        if let Mode::Hash(hashed) = &mut self.mode {
            return hashed.next().await.transpose();
        }
        if self.source.is_none() {
            self.start().await?;
        }
        let left = match self.pending.pop_front() {
            Some(b) => b,
            None if self.left_done => return Ok(None),
            None => match self.left.next().await {
                Some(b) => b?,
                None => return Ok(None),
            },
        };
        self.probe(&left).await.map(Some)
    }
}

/// Return the conditions of a right input (filters over a scan), from the scan up.
fn right_conditions(node: &Physical) -> Vec<Expr> {
    match &node.op {
        Op::TableScan { pushed, policy, .. } => {
            // Policy conjuncts first, as storage evaluates them (`exec::start`).
            policy.iter().chain(pushed.iter()).cloned().collect()
        }
        Op::Filter { input, predicate }
        | Op::PolicyFilter {
            input,
            residual: predicate,
            ..
        } => {
            let mut below = right_conditions(input);
            below.push(predicate.clone());
            below
        }
        _ => Vec::new(),
    }
}

/// The parts of an [`Op::IndexLookup`] the operator needs.
pub(crate) struct LookupSpec<'a> {
    /// The join kind.
    pub(crate) kind: JoinKind,
    /// The left input.
    pub(crate) left: &'a Physical,
    /// The right input as planned.
    pub(crate) right: &'a Physical,
    /// The right table.
    pub(crate) table: &'a Arc<TableDef>,
    /// The key the right rows are reached by.
    pub(crate) key: &'a LookupKey,
    /// The equality keys, pinned columns first.
    pub(crate) keys: &'a [(Expr, Expr)],
    /// How many leading key columns are pinned.
    pub(crate) prefix: usize,
    /// Whether the pinned columns are a whole unique primary key.
    pub(crate) point: bool,
    /// The residual condition.
    pub(crate) residual: Option<&'a Expr>,
    /// Rows per storage scan batch.
    pub(crate) batch_rows: usize,
}

/// Start an index lookup join producing `node`'s schema.
pub(crate) fn index_lookup(
    node: &Physical,
    spec: &LookupSpec<'_>,
    ctx: &ExecContext,
) -> Result<BatchStream, ExecError> {
    let (left, right) = (spec.left, spec.right);
    let left_keys = spec
        .keys
        .iter()
        .map(|(l, _)| compile_over(l, &left.layout, &left.types, ctx))
        .collect::<Result<_, _>>()?;
    let right_keys = spec
        .keys
        .iter()
        .map(|(_, r)| compile_over(r, &right.layout, &right.types, ctx))
        .collect::<Result<_, _>>()?;
    let stages = right_conditions(right)
        .iter()
        .map(|e| compile_over(e, &right.layout, &right.types, ctx))
        .collect::<Result<_, _>>()?;
    let combined_layout: Vec<_> = left.layout.iter().chain(&right.layout).copied().collect();
    let combined_types: Vec<_> = left.types.iter().chain(&right.types).cloned().collect();
    let residual = spec
        .residual
        .map(|r| compile_over(r, &combined_layout, &combined_types, ctx))
        .transpose()?;
    Ok(into_stream(LookupState {
        mode: Mode::Undecided,
        pending: VecDeque::new(),
        left_done: false,
        lookup_rows: LOOKUP_ROWS,
        fallback: Fallback {
            node: node.clone(),
            left: left.clone(),
            right: right.clone(),
            keys: spec.keys.to_vec(),
            residual: spec.residual.cloned(),
        },
        kind: spec.kind,
        left: execute(left, ctx),
        ctx: ctx.clone(),
        table: spec.table.clone(),
        key: spec.key.clone(),
        prefix: spec.prefix,
        point: spec.point,
        source: None,
        left_keys,
        right_keys,
        stages,
        residual,
        right_types: right.types.clone(),
        right_schema: right.schema.clone(),
        schema: node.schema.clone(),
        batch_rows: spec.batch_rows.max(1),
        memory: ctx.reserve(node, "IndexLookup"),
        metrics: ctx.node_metrics(node.id),
    }))
}
