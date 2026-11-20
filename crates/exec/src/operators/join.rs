//! `Join`: hash join on equality keys, or nested-loop join without them.
//!
//! The right input is read completely and indexed by its key values; left
//! batches then probe it. Key-matching pairs are evaluated against the
//! residual condition with the kernels, in chunks, and each kind assembles
//! its output from the pairs that pass. NULL keys never match, as with `=`.
//! Semi and anti joins stop at a left row's first passing pair. Reading the
//! build side yields to the runtime every few batches (`coop`).

use std::collections::HashMap;

use arrow_array::{Array, ArrayRef, BooleanArray, RecordBatch, UInt32Array};
use arrow_schema::SchemaRef;
use interlaken_ir::JoinKind;
use interlaken_ir::interp::RowKey;

use futures::StreamExt;

use super::pull::{Pull, batch, concat, into_stream, row_keys, take};
use crate::coop::Yielder;
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over, execute};
use crate::kernels::CompiledExpr;
use crate::memory::{Reservation, batch_bytes, key_bytes, vec_bytes};
use crate::physical::{BATCH_ROWS, Physical};

/// The most pairs evaluated against the residual at once.
const PAIR_CHUNK: usize = 4096;

/// Sentinel ending a chain of right rows with equal keys.
const END: u32 = u32::MAX;

/// The right rows with each key, as a chain through [`Index::next`].
///
/// A row per key costs a table entry (the key and two row numbers) plus
/// four bytes, instead of a vector of rows per key.
struct Index {
    /// The first and last right row with each key (keys with a NULL are left out).
    chains: HashMap<RowKey, (u32, u32)>,
    /// The next right row with the same key as each row, or [`END`].
    next: Vec<u32>,
}

impl Index {
    /// Return the right rows with `key`, in row order.
    fn rows(&self, key: &RowKey) -> Vec<u32> {
        let mut out = Vec::new();
        let Some(&(first, _)) = self.chains.get(key) else {
            return out;
        };
        let mut row = first;
        while row != END {
            out.push(row);
            row = self.next[row as usize];
        }
        out
    }
}

/// The built right side.
struct Build {
    /// Every right row.
    rows: RecordBatch,
    /// Right rows by key value; `None` for a nested-loop join.
    index: Option<Index>,
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
    ///
    /// The batches read and their concatenation exist together for a
    /// moment, and the reservation covers both. Keys are evaluated and
    /// indexed a batch's worth of rows at a time.
    async fn build(&mut self) -> Result<(), ExecError> {
        let mut right = self.right.take().expect("built once");
        let mut batches = Vec::new();
        let mut held = 0;
        let mut yielder = Yielder::default();
        while let Some(b) = right.next().await {
            let b = b?;
            yielder.tick().await;
            let bytes = batch_bytes(&b);
            self.memory.grow(bytes)?;
            held += bytes;
            batches.push(b);
        }
        let rows = if batches.len() == 1 {
            batches.pop().expect("one batch")
        } else {
            // Concatenation copies each batch's own rows.
            self.memory.grow(held)?;
            let rows = concat(&self.right_schema, &batches)?;
            drop(batches);
            rows
        };
        let n = rows.num_rows();
        self.memory.resize(batch_bytes(&rows) + n)?;
        let matched = vec![false; n];
        let index = if self.right_keys.is_empty() {
            None
        } else {
            Some(self.index(&rows)?)
        };
        self.build = Some(Build {
            rows,
            index,
            matched,
        });
        Ok(())
    }

    /// Index `rows` by their key values, reserving for the index as it grows.
    fn index(&mut self, rows: &RecordBatch) -> Result<Index, ExecError> {
        let n = rows.num_rows();
        self.memory.grow(vec_bytes::<u32>(n))?;
        let mut index = Index {
            chains: HashMap::new(),
            next: vec![END; n],
        };
        for start in (0..n).step_by(BATCH_ROWS) {
            let len = BATCH_ROWS.min(n - start);
            let chunk = rows.slice(start, len);
            let keys = self
                .right_keys
                .iter_mut()
                .map(|k| k.eval(chunk.columns(), len))
                .collect::<Result<Vec<_>, _>>()?;
            for (i, key) in row_keys(&keys, len)?.into_iter().enumerate() {
                if key.0.iter().any(interlaken_ir::interp::Datum::is_null) {
                    continue;
                }
                let j = u32::try_from(start + i).expect("build rows fit in u32");
                if let Some((_, last)) = index.chains.get_mut(&key) {
                    index.next[*last as usize] = j;
                    *last = j;
                    continue;
                }
                let replaced = self.memory.reserve_entry::<(RowKey, (u32, u32))>(
                    index.chains.len(),
                    index.chains.capacity(),
                    key_bytes(&key),
                )?;
                index.chains.insert(key, (j, j));
                self.memory.shrink(replaced);
            }
        }
        Ok(index)
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
                    index.rows(&key)
                }
            })
            .collect())
    }

    /// Probe with one left batch and return its output.
    fn probe(&mut self, left: &RecordBatch) -> Result<RecordBatch, ExecError> {
        let candidates = self.candidates(left)?;
        let build = self.build.as_mut().expect("built");
        pair_up(
            &mut Pairing {
                kind: self.kind,
                residual: self.residual.as_mut(),
                schema: &self.schema,
                right: &build.rows,
                right_matched: Some(&mut build.matched),
            },
            left,
            &candidates,
        )
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

/// What a join needs to turn candidate pairs into output.
pub(crate) struct Pairing<'a> {
    /// The join kind.
    pub(crate) kind: JoinKind,
    /// The residual condition over left then right columns.
    pub(crate) residual: Option<&'a mut CompiledExpr>,
    /// The output schema.
    pub(crate) schema: &'a SchemaRef,
    /// The right rows the candidates index.
    pub(crate) right: &'a RecordBatch,
    /// Which right rows matched some left row, for right and full joins.
    pub(crate) right_matched: Option<&'a mut Vec<bool>>,
}

/// Return the combined left-then-right columns of `pairs`.
fn combine(
    left: &RecordBatch,
    right: &RecordBatch,
    pairs: &[(u32, u32)],
) -> Result<Vec<ArrayRef>, ExecError> {
    let li: UInt32Array = pairs.iter().map(|p| p.0).collect();
    let ri: UInt32Array = pairs.iter().map(|p| p.1).collect();
    let mut columns = take(left.columns(), &li)?;
    columns.extend(take(right.columns(), &ri)?);
    Ok(columns)
}

/// Return which of `pairs` satisfy the residual condition.
fn passing(
    pairing: &mut Pairing<'_>,
    left: &RecordBatch,
    pairs: &[(u32, u32)],
) -> Result<Vec<bool>, ExecError> {
    let Some(residual) = pairing.residual.as_deref_mut() else {
        return Ok(vec![true; pairs.len()]);
    };
    let columns = combine(left, pairing.right, pairs)?;
    let result = residual.eval(&columns, pairs.len())?;
    let result = result
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| ExecError::Unsupported("non-boolean join condition".into()))?;
    Ok((0..pairs.len())
        .map(|i| result.is_valid(i) && result.value(i))
        .collect())
}

/// Return the output of left rows `rows` padded with NULLs on the right.
fn left_padded(
    pairing: &Pairing<'_>,
    left: &RecordBatch,
    rows: &[u32],
) -> Result<RecordBatch, ExecError> {
    let li: UInt32Array = rows.iter().copied().collect();
    let nulls: UInt32Array = rows.iter().map(|_| None::<u32>).collect();
    let mut columns = take(left.columns(), &li)?;
    columns.extend(take(pairing.right.columns(), &nulls)?);
    batch(pairing.schema, columns, rows.len())
}

/// Return a join's output for one left batch, given each left row's candidate right rows.
///
/// Candidates are evaluated against the residual in chunks. Semi and anti
/// joins try each row's candidates in rounds, so a row stops at its first
/// passing pair; left and full joins pad unmatched left rows.
pub(crate) fn pair_up(
    pairing: &mut Pairing<'_>,
    left: &RecordBatch,
    candidates: &[Vec<u32>],
) -> Result<RecordBatch, ExecError> {
    let mut matched = vec![false; left.num_rows()];
    let mut out: Vec<RecordBatch> = Vec::new();
    if matches!(pairing.kind, JoinKind::Semi | JoinKind::Anti) {
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
                for (pair, pass) in chunk.iter().zip(passing(pairing, left, chunk)?) {
                    matched[pair.0 as usize] |= pass;
                }
            }
            round += 1;
        }
        let keep = pairing.kind == JoinKind::Semi;
        let rows: UInt32Array = (0..left.num_rows())
            .filter(|&i| matched[i] == keep)
            .map(|i| u32::try_from(i).expect("rows fit in u32"))
            .collect();
        let columns = take(left.columns(), &rows)?;
        return batch(pairing.schema, columns, rows.len());
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
        let pass = passing(pairing, left, chunk)?;
        let kept: Vec<(u32, u32)> = chunk
            .iter()
            .zip(&pass)
            .filter(|(_, p)| **p)
            .map(|(pair, _)| *pair)
            .collect();
        for &(i, j) in &kept {
            matched[i as usize] = true;
            if let Some(right_matched) = pairing.right_matched.as_deref_mut() {
                right_matched[j as usize] = true;
            }
        }
        out.push(batch(
            pairing.schema,
            combine(left, pairing.right, &kept)?,
            kept.len(),
        )?);
    }
    if matches!(pairing.kind, JoinKind::Left | JoinKind::Full) {
        let unmatched: Vec<u32> = (0..left.num_rows())
            .filter(|&i| !matched[i])
            .map(|i| u32::try_from(i).expect("rows fit in u32"))
            .collect();
        out.push(left_padded(pairing, left, &unmatched)?);
    }
    concat(pairing.schema, &out)
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
    hash_join(
        node,
        kind,
        (execute(left, ctx), execute(right, ctx)),
        left,
        right,
        &JoinCondition { keys, residual },
        ctx,
    )
}

/// A join's condition: equality keys and the rest.
pub(crate) struct JoinCondition<'a> {
    /// Equality keys: (left expression, right expression).
    pub(crate) keys: &'a [(interlaken_ir::Expr, interlaken_ir::Expr)],
    /// The rest of the condition, evaluated on key-matching pairs.
    pub(crate) residual: Option<&'a interlaken_ir::Expr>,
}

/// Start a hash join (or a nested-loop one, without keys) of the batches
/// `inputs`, laid out as `left`'s and `right`'s outputs, producing `node`'s
/// schema.
pub(crate) fn hash_join(
    node: &Physical,
    kind: JoinKind,
    inputs: (BatchStream, BatchStream),
    left: &Physical,
    right: &Physical,
    on: &JoinCondition<'_>,
    ctx: &ExecContext,
) -> Result<BatchStream, ExecError> {
    let left_keys = on
        .keys
        .iter()
        .map(|(l, _)| compile_over(l, &left.layout, &left.types, ctx))
        .collect::<Result<_, _>>()?;
    let right_keys = on
        .keys
        .iter()
        .map(|(_, r)| compile_over(r, &right.layout, &right.types, ctx))
        .collect::<Result<_, _>>()?;
    let combined_layout: Vec<_> = left.layout.iter().chain(&right.layout).copied().collect();
    let combined_types: Vec<_> = left.types.iter().chain(&right.types).cloned().collect();
    let residual = on
        .residual
        .map(|r| compile_over(r, &combined_layout, &combined_types, ctx))
        .transpose()?;
    let (left_batches, right_batches) = inputs;
    Ok(into_stream(JoinState {
        kind,
        left: left_batches,
        right: Some(right_batches),
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
