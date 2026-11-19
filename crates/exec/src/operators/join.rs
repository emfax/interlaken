//! `Join`: hash join on equality keys, or nested-loop join without them.
//!
//! The right input is read completely and indexed by its key values; left
//! batches then probe it. Key-matching pairs are evaluated against the
//! residual condition with the kernels, in chunks, and each kind assembles
//! its output from the pairs that pass. NULL keys never match, as with `=`.
//! Semi and anti joins stop at a left row's first passing pair.

use std::collections::HashMap;

use arrow_array::{Array, ArrayRef, BooleanArray, RecordBatch, UInt32Array};
use arrow_schema::SchemaRef;
use interlaken_ir::JoinKind;
use interlaken_ir::interp::RowKey;

use futures::StreamExt;

use super::pull::{Pull, batch, concat, into_stream, row_keys, take};
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over, execute};
use crate::kernels::CompiledExpr;
use crate::memory::{Reservation, batch_bytes, entry_bytes};
use crate::physical::Physical;

/// The most pairs evaluated against the residual at once.
const PAIR_CHUNK: usize = 4096;

/// The built right side.
struct Build {
    /// Every right row.
    rows: RecordBatch,
    /// Right rows by key value; `None` for a nested-loop join.
    index: Option<HashMap<RowKey, Vec<u32>>>,
    /// Which right rows matched some left row (right and full joins).
    matched: Vec<bool>,
}

/// A join in progress.
struct JoinState {
    /// The join kind.
    kind: JoinKind,
    /// The probe input.
    left: BatchStream,
    /// The build input, until it is read.
    right: Option<BatchStream>,
    /// The build side's schema.
    right_schema: SchemaRef,
    /// The built right side.
    build: Option<Build>,
    /// Left key expressions.
    left_keys: Vec<CompiledExpr>,
    /// Right key expressions.
    right_keys: Vec<CompiledExpr>,
    /// The residual condition over left then right columns.
    residual: Option<CompiledExpr>,
    /// The output schema.
    schema: SchemaRef,
    /// Whether the left input is exhausted.
    left_done: bool,
    /// Whether unmatched right rows were emitted.
    finished: bool,
    /// The memory held by the build side.
    memory: Reservation,
}

impl JoinState {
    /// Read and index the right input.
    async fn build(&mut self) -> Result<(), ExecError> {
        let mut right = self.right.take().expect("built once");
        let mut batches = Vec::new();
        while let Some(b) = right.next().await {
            let b = b?;
            self.memory.grow(batch_bytes(&b))?;
            batches.push(b);
        }
        let rows = concat(&self.right_schema, &batches)?;
        drop(batches);
        self.memory.resize(batch_bytes(&rows) + rows.num_rows())?;
        let index = if self.right_keys.is_empty() {
            None
        } else {
            let keys = self
                .right_keys
                .iter_mut()
                .map(|k| k.eval(rows.columns(), rows.num_rows()))
                .collect::<Result<Vec<_>, _>>()?;
            let mut index: HashMap<RowKey, Vec<u32>> = HashMap::new();
            for (j, key) in row_keys(&keys, rows.num_rows())?.into_iter().enumerate() {
                if !key.0.iter().any(interlaken_ir::interp::Datum::is_null) {
                    self.memory
                        .grow(entry_bytes(&key, std::mem::size_of::<u32>()))?;
                    index
                        .entry(key)
                        .or_default()
                        .push(u32::try_from(j).expect("build rows fit in u32"));
                }
            }
            Some(index)
        };
        let matched = vec![false; rows.num_rows()];
        self.build = Some(Build {
            rows,
            index,
            matched,
        });
        Ok(())
    }

    /// Return each left row's candidate right rows.
    fn candidates(&mut self, left: &RecordBatch) -> Result<Vec<Vec<u32>>, ExecError> {
        let build = self.build.as_ref().expect("built");
        let Some(index) = &build.index else {
            let all: Vec<u32> =
                (0..u32::try_from(build.rows.num_rows()).expect("build rows fit in u32")).collect();
            return Ok(vec![all; left.num_rows()]);
        };
        let keys = self
            .left_keys
            .iter_mut()
            .map(|k| k.eval(left.columns(), left.num_rows()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(row_keys(&keys, left.num_rows())?
            .into_iter()
            .map(|key| {
                if key.0.iter().any(interlaken_ir::interp::Datum::is_null) {
                    Vec::new()
                } else {
                    index.get(&key).cloned().unwrap_or_default()
                }
            })
            .collect())
    }

    /// Return the combined left-then-right columns of `pairs`.
    fn combine(
        &self,
        left: &RecordBatch,
        pairs: &[(u32, u32)],
    ) -> Result<Vec<ArrayRef>, ExecError> {
        let build = self.build.as_ref().expect("built");
        let li: UInt32Array = pairs.iter().map(|p| p.0).collect();
        let ri: UInt32Array = pairs.iter().map(|p| p.1).collect();
        let mut columns = take(left.columns(), &li)?;
        columns.extend(take(build.rows.columns(), &ri)?);
        Ok(columns)
    }

    /// Return which of `pairs` satisfy the residual condition.
    fn passing(
        &mut self,
        left: &RecordBatch,
        pairs: &[(u32, u32)],
    ) -> Result<Vec<bool>, ExecError> {
        if self.residual.is_none() {
            return Ok(vec![true; pairs.len()]);
        }
        let columns = self.combine(left, pairs)?;
        let residual = self.residual.as_mut().expect("checked above");
        let result = residual.eval(&columns, pairs.len())?;
        let result = result
            .as_any()
            .downcast_ref::<BooleanArray>()
            .ok_or_else(|| ExecError::Unsupported("non-boolean join condition".into()))?;
        Ok((0..pairs.len())
            .map(|i| result.is_valid(i) && result.value(i))
            .collect())
    }

    /// Return the output of a left row padded with NULLs on the right, for `rows`.
    fn left_padded(&self, left: &RecordBatch, rows: &[u32]) -> Result<RecordBatch, ExecError> {
        let build = self.build.as_ref().expect("built");
        let li: UInt32Array = rows.iter().copied().collect();
        let nulls: UInt32Array = rows.iter().map(|_| None::<u32>).collect();
        let mut columns = take(left.columns(), &li)?;
        columns.extend(take(build.rows.columns(), &nulls)?);
        batch(&self.schema, columns, rows.len())
    }

    /// Probe with one left batch and return its output.
    fn probe(&mut self, left: &RecordBatch) -> Result<RecordBatch, ExecError> {
        let candidates = self.candidates(left)?;
        let mut matched = vec![false; left.num_rows()];
        let mut out: Vec<RecordBatch> = Vec::new();
        if matches!(self.kind, JoinKind::Semi | JoinKind::Anti) {
            // Round r tries each unmatched row's r-th candidate, so a row stops at its first match.
            let mut round = 0;
            loop {
                let pairs: Vec<(u32, u32)> = candidates
                    .iter()
                    .enumerate()
                    .filter(|(i, c)| !matched[*i] && round < c.len())
                    .map(|(i, c)| (u32::try_from(i).expect("rows fit in u32"), c[round]))
                    .collect();
                if pairs.is_empty() {
                    break;
                }
                for chunk in pairs.chunks(PAIR_CHUNK) {
                    for (pair, pass) in chunk.iter().zip(self.passing(left, chunk)?) {
                        matched[pair.0 as usize] |= pass;
                    }
                }
                round += 1;
            }
            let keep = self.kind == JoinKind::Semi;
            let rows: UInt32Array = (0..left.num_rows())
                .filter(|&i| matched[i] == keep)
                .map(|i| u32::try_from(i).expect("rows fit in u32"))
                .collect();
            let columns = take(left.columns(), &rows)?;
            return batch(&self.schema, columns, rows.len());
        }
        let pairs: Vec<(u32, u32)> = candidates
            .iter()
            .enumerate()
            .flat_map(|(i, c)| {
                c.iter()
                    .map(move |&j| (u32::try_from(i).expect("rows fit in u32"), j))
            })
            .collect();
        for chunk in pairs.chunks(PAIR_CHUNK) {
            let pass = self.passing(left, chunk)?;
            let kept: Vec<(u32, u32)> = chunk
                .iter()
                .zip(&pass)
                .filter(|(_, p)| **p)
                .map(|(pair, _)| *pair)
                .collect();
            let build = self.build.as_mut().expect("built");
            for &(i, j) in &kept {
                matched[i as usize] = true;
                build.matched[j as usize] = true;
            }
            out.push(batch(&self.schema, self.combine(left, &kept)?, kept.len())?);
        }
        if matches!(self.kind, JoinKind::Left | JoinKind::Full) {
            let unmatched: Vec<u32> = (0..left.num_rows())
                .filter(|&i| !matched[i])
                .map(|i| u32::try_from(i).expect("rows fit in u32"))
                .collect();
            out.push(self.left_padded(left, &unmatched)?);
        }
        concat(&self.schema, &out)
    }

    /// Return the right rows no left row matched, padded with NULLs on the left.
    fn unmatched_right(&self, left_schema_width: usize) -> Result<RecordBatch, ExecError> {
        let build = self.build.as_ref().expect("built");
        let rows: Vec<u32> = (0..build.rows.num_rows())
            .filter(|&j| !build.matched[j])
            .map(|j| u32::try_from(j).expect("rows fit in u32"))
            .collect();
        let ri: UInt32Array = rows.iter().copied().collect();
        let mut columns: Vec<ArrayRef> = self.schema.fields()[..left_schema_width]
            .iter()
            .map(|f| arrow_array::new_null_array(f.data_type(), rows.len()))
            .collect();
        columns.extend(take(build.rows.columns(), &ri)?);
        batch(&self.schema, columns, rows.len())
    }
}

impl Pull for JoinState {
    /// Build on the first pull, then probe one left batch per pull.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        use futures::StreamExt;
        if self.build.is_none() {
            self.build().await?;
        }
        if !self.left_done {
            match self.left.next().await {
                Some(left) => return self.probe(&left?).map(Some),
                None => self.left_done = true,
            }
        }
        if !self.finished && matches!(self.kind, JoinKind::Right | JoinKind::Full) {
            self.finished = true;
            let width = self.schema.fields().len() - self.right_schema.fields().len();
            return self.unmatched_right(width).map(Some);
        }
        Ok(None)
    }
}

/// Start a join of `left` and `right` producing `node`'s schema.
pub(crate) fn join(
    node: &Physical,
    kind: JoinKind,
    left: &Physical,
    right: &Physical,
    keys: &[(interlaken_ir::Expr, interlaken_ir::Expr)],
    residual: Option<&interlaken_ir::Expr>,
    ctx: &ExecContext,
) -> Result<BatchStream, ExecError> {
    let left_keys = keys
        .iter()
        .map(|(l, _)| compile_over(l, &left.layout, &left.types, ctx))
        .collect::<Result<_, _>>()?;
    let right_keys = keys
        .iter()
        .map(|(_, r)| compile_over(r, &right.layout, &right.types, ctx))
        .collect::<Result<_, _>>()?;
    let combined_layout: Vec<_> = left.layout.iter().chain(&right.layout).copied().collect();
    let combined_types: Vec<_> = left.types.iter().chain(&right.types).cloned().collect();
    let residual = residual
        .map(|r| compile_over(r, &combined_layout, &combined_types, ctx))
        .transpose()?;
    Ok(into_stream(JoinState {
        kind,
        left: execute(left, ctx),
        right: Some(execute(right, ctx)),
        right_schema: right.schema.clone(),
        build: None,
        left_keys,
        right_keys,
        residual,
        schema: node.schema.clone(),
        left_done: false,
        finished: false,
        memory: ctx.reserve(node, "HashJoin"),
    }))
}
