//! `Sort`: an in-memory sort, or top-N when a limit sits directly above.
//!
//! Rows are ordered with the interpreter's sort-key comparison, stably.
//! With a fetch of N rows, the operator sorts and truncates to N whenever
//! it has buffered enough, so memory stays proportional to N. Reading the
//! input yields to the runtime every few batches (`coop`).

use arrow_array::{RecordBatch, UInt32Array};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use interlaken_ir::interp::{Datum, compare_sort_keys};
use interlaken_ir::{Expr, SortKey};

use super::pull::{Pull, batch, chunks, concat, into_stream, take};
use crate::columnar::datum_at;
use crate::coop::Yielder;
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over, execute};
use crate::kernels::CompiledExpr;
use crate::memory::{Reservation, batch_bytes};
use crate::physical::{BATCH_ROWS, Physical};
use crate::rowwise::eval_constant;

/// A sort in progress.
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
    /// The memory held by buffered rows.
    memory: Reservation,
    /// Sorted output batches not yet returned, last first.
    ready: Vec<RecordBatch>,
}

impl SortState {
    /// Sort `rows` and keep at most `fetch` of them.
    fn sort(&mut self, rows: &RecordBatch) -> Result<RecordBatch, ExecError> {
        let n = rows.num_rows();
        let keys = self
            .exprs
            .iter_mut()
            .map(|e| e.eval(rows.columns(), n))
            .collect::<Result<Vec<_>, _>>()?;
        let values: Vec<Vec<Datum>> = (0..n)
            .map(|r| {
                keys.iter()
                    .map(|k| datum_at(k.as_ref(), r))
                    .collect::<Result<_, _>>()
            })
            .collect::<Result<_, _>>()?;
        let mut order: Vec<u32> = (0..u32::try_from(n).expect("rows fit in u32")).collect();
        order.sort_by(|&a, &b| {
            compare_sort_keys(&values[a as usize], &values[b as usize], &self.keys)
        });
        if let Some(fetch) = self.fetch {
            order.truncate(fetch);
        }
        let indices = UInt32Array::from(order);
        let columns = take(rows.columns(), &indices)?;
        batch(&self.schema, columns, indices.len())
    }

    /// Consume the input and sort it.
    async fn run(&mut self) -> Result<(), ExecError> {
        let mut input = self.input.take().expect("run once");
        let mut buffered: Vec<RecordBatch> = Vec::new();
        let mut rows = 0;
        let mut yielder = Yielder::default();
        while let Some(b) = input.next().await {
            let b = b?;
            yielder.tick().await;
            self.memory.grow(batch_bytes(&b))?;
            rows += b.num_rows();
            buffered.push(b);
            // Top-N: once twice the fetch is buffered, keep only the best rows.
            if let Some(fetch) = self.fetch
                && rows > (2 * fetch).max(BATCH_ROWS)
            {
                let all = concat(&self.schema, &buffered)?;
                let best = self.sort(&all)?;
                rows = best.num_rows();
                self.memory.resize(batch_bytes(&best))?;
                buffered = vec![best];
            }
        }
        let all = concat(&self.schema, &buffered)?;
        drop(buffered);
        let sorted = self.sort(&all)?;
        drop(all);
        self.memory.resize(batch_bytes(&sorted))?;
        self.ready = chunks(&sorted);
        self.ready.reverse();
        Ok(())
    }
}

impl Pull for SortState {
    /// Sort everything on the first pull, then return it in batches.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if self.input.is_some() {
            self.run().await?;
        }
        Ok(self.ready.pop())
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
        ready: Vec::new(),
    }))
}
