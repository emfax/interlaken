//! `Sort`: an in-memory sort, or top-N when a limit sits directly above.
//!
//! Rows are ordered with the interpreter's sort-key comparison, stably.
//! The input batches are kept as they arrive, with their key values; the
//! sort orders row numbers, and the output is gathered from the kept
//! batches a batch at a time. So a sort holds its input once, plus its key
//! values while it sorts, and its output batches are compact rather than
//! slices of one large buffer. With a fetch of N rows, the operator sorts
//! and truncates to N whenever it has buffered enough, so memory stays
//! proportional to N. Reading the input yields to the runtime every few
//! batches (`coop`).

use arrow_array::{Array, RecordBatch};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use interlaken_ir::interp::{Datum, compare_sort_keys};
use interlaken_ir::{Expr, SortKey};

use super::pull::{Pull, batch, into_stream};
use crate::columnar::datum_at;
use crate::coop::Yielder;
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over, execute};
use crate::kernels::CompiledExpr;
use crate::memory::{Reservation, batch_bytes, datum_heap_bytes, vec_bytes};
use crate::physical::{BATCH_ROWS, Physical};
use crate::rowwise::eval_constant;

/// A sort in progress.
///
/// Input batches are kept as they arrive, with their rows' sort key values
/// in one flat vector. Sorting orders row numbers; the output is gathered
/// from the kept batches a batch at a time, so the input is never
/// concatenated and no sorted copy of it exists at once.
struct SortState {
    /// The input, until it is consumed.
    input: Option<BatchStream>,
    /// The sort keys.
    keys: Vec<SortKey>,
    /// The compiled key expressions.
    exprs: Vec<CompiledExpr>,
    /// The most rows any consumer can use, if bounded.
    fetch: Option<usize>,
    /// The schema.
    schema: SchemaRef,
    /// The memory held by kept batches, key values and the order.
    memory: Reservation,
    /// The kept input batches.
    batches: Vec<RecordBatch>,
    /// The first row number of each kept batch, then the total.
    starts: Vec<usize>,
    /// The sort key values of every kept row, row after row.
    values: Vec<Datum>,
    /// The kept rows' numbers in sorted order, once sorted.
    order: Vec<u32>,
    /// How many rows of `order` were returned.
    emitted: usize,
}

impl SortState {
    /// Return the rows kept.
    fn rows(&self) -> usize {
        *self.starts.last().expect("starts with 0")
    }

    /// Keep batch `b` and its key values, reserving for both.
    fn keep(&mut self, b: RecordBatch) -> Result<(), ExecError> {
        let n = b.num_rows();
        let keys = self
            .exprs
            .iter_mut()
            .map(|e| e.eval(b.columns(), n))
            .collect::<Result<Vec<_>, _>>()?;
        let wanted = self.values.len() + n * keys.len();
        let bytes = batch_bytes(&b);
        if wanted > self.values.capacity() {
            // The values move to a buffer twice the size; both exist while they do.
            let grown = (2 * self.values.capacity()).max(wanted);
            let old = vec_bytes::<Datum>(self.values.capacity());
            self.memory.grow(bytes + vec_bytes::<Datum>(grown))?;
            self.values.reserve_exact(grown - self.values.len());
            self.memory.shrink(old);
        } else {
            self.memory.grow(bytes)?;
        }
        let mut heap = 0;
        for r in 0..n {
            for k in &keys {
                let value = datum_at(k.as_ref(), r)?;
                heap += datum_heap_bytes(&value);
                self.values.push(value);
            }
        }
        self.memory.grow(heap)?;
        self.starts.push(self.rows() + n);
        self.batches.push(b);
        Ok(())
    }

    /// Order the kept rows, keeping at most `fetch` of them in `order`.
    fn sort(&mut self) -> Result<(), ExecError> {
        let n = self.rows();
        let width = self.keys.len();
        // A stable sort of n row numbers uses scratch space for up to n more.
        let scratch = vec_bytes::<u32>(n);
        self.memory.grow(vec_bytes::<u32>(n) + scratch)?;
        let mut order: Vec<u32> = (0..u32::try_from(n).expect("rows fit in u32")).collect();
        let values = &self.values;
        let row = |r: u32| &values[r as usize * width..(r as usize + 1) * width];
        order.sort_by(|&a, &b| compare_sort_keys(row(a), row(b), &self.keys));
        self.memory.shrink(scratch);
        if let Some(fetch) = self.fetch {
            order.truncate(fetch);
        }
        self.order = order;
        Ok(())
    }

    /// Gather the kept rows `order[range]` into one batch.
    fn gather(&self, range: std::ops::Range<usize>) -> Result<RecordBatch, ExecError> {
        let indices: Vec<(usize, usize)> = self.order[range]
            .iter()
            .map(|&r| {
                let r = r as usize;
                let b = self.starts.partition_point(|&s| s <= r) - 1;
                (b, r - self.starts[b])
            })
            .collect();
        let columns = (0..self.schema.fields().len())
            .map(|c| {
                let arrays: Vec<&dyn Array> =
                    self.batches.iter().map(|b| b.column(c).as_ref()).collect();
                Ok(arrow_select::interleave::interleave(&arrays, &indices)?)
            })
            .collect::<Result<Vec<_>, ExecError>>()?;
        batch(&self.schema, columns, indices.len())
    }

    /// Release everything kept and reserved.
    fn clear(&mut self) {
        self.batches.clear();
        self.starts.truncate(1);
        self.values = Vec::new();
        self.order = Vec::new();
        self.memory.shrink(usize::MAX);
    }

    /// Top-N: keep only the best `fetch` rows, as one batch with their key values.
    fn compact(&mut self) -> Result<(), ExecError> {
        self.sort()?;
        let best = self.gather(0..self.order.len())?;
        let width = self.keys.len();
        let mut values = Vec::with_capacity(self.order.len() * width);
        for &r in &self.order {
            let r = r as usize;
            values.extend_from_slice(&self.values[r * width..(r + 1) * width]);
        }
        self.clear();
        let heap: usize = values.iter().map(datum_heap_bytes).sum();
        self.memory
            .grow(batch_bytes(&best) + vec_bytes::<Datum>(values.capacity()) + heap)?;
        self.values = values;
        self.starts.push(best.num_rows());
        self.batches.push(best);
        Ok(())
    }

    /// Consume the input and sort it.
    async fn run(&mut self) -> Result<(), ExecError> {
        let mut input = self.input.take().expect("run once");
        let mut yielder = Yielder::default();
        while let Some(b) = input.next().await {
            let b = b?;
            yielder.tick().await;
            self.keep(b)?;
            // Top-N: once twice the fetch is kept, keep only the best rows.
            if let Some(fetch) = self.fetch
                && self.rows() > (2 * fetch).max(BATCH_ROWS)
            {
                self.compact()?;
            }
        }
        self.sort()?;
        // The key values are needed only to sort.
        let held: usize = vec_bytes::<Datum>(self.values.capacity())
            + self.values.iter().map(datum_heap_bytes).sum::<usize>();
        self.values = Vec::new();
        self.memory.shrink(held);
        Ok(())
    }
}

impl Pull for SortState {
    /// Sort everything on the first pull, then return it in batches.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if self.input.is_some() {
            self.run().await?;
        }
        let start = self.emitted;
        let end = (start + BATCH_ROWS).min(self.order.len());
        if start == end {
            self.clear();
            return Ok(None);
        }
        self.emitted = end;
        self.gather(start..end).map(Some)
    }
}

/// Evaluate a limit or offset for a fetch bound; `None` when absent, NULL or invalid.
///
/// Invalid values are left for the `Limit` above to report.
fn bound(expr: Option<&Expr>, ctx: &ExecContext) -> Option<Option<usize>> {
    let Some(expr) = expr else { return Some(None) };
    match eval_constant(ctx, expr) {
        Ok(Datum::Null) => Some(None),
        Ok(Datum::Int8(n)) if n >= 0 => Some(Some(usize::try_from(n).unwrap_or(usize::MAX))),
        _ => None,
    }
}

/// Start sorting `input` by `keys`, keeping only what `fetch` (limit, offset) needs.
pub(crate) fn sort(
    node: &Physical,
    input: &Physical,
    keys: &[SortKey],
    fetch: Option<&(Option<Expr>, Option<Expr>)>,
    ctx: &ExecContext,
) -> Result<BatchStream, ExecError> {
    let exprs = keys
        .iter()
        .map(|k| compile_over(&k.expr, &input.layout, &input.types, ctx))
        .collect::<Result<_, _>>()?;
    let fetch = fetch.and_then(|(limit, offset)| {
        let limit = bound(limit.as_ref(), ctx)??;
        let offset = bound(offset.as_ref(), ctx)?.unwrap_or(0);
        Some(limit.saturating_add(offset))
    });
    Ok(into_stream(SortState {
        input: Some(execute(input, ctx)),
        keys: keys.to_vec(),
        exprs,
        fetch,
        schema: node.schema.clone(),
        memory: ctx.reserve(node, if fetch.is_some() { "TopN" } else { "Sort" }),
        batches: Vec::new(),
        starts: vec![0],
        values: Vec::new(),
        order: Vec::new(),
        emitted: 0,
    }))
}
