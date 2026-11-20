//! `Aggregate`: hash aggregation.
//!
//! Group keys are evaluated for every row; each aggregate's arguments only
//! for the rows its `FILTER` keeps, as in the interpreter. `count`, `sum`,
//! `avg`, `min`, `max`, `bool_and` and `bool_or` fold rows into a fixed-size
//! state as they arrive, exactly as the interpreter would compute them
//! (`accumulate`), so memory grows with the groups, not the rows. `DISTINCT`
//! and other aggregates keep their argument tuples and are finished by the
//! interpreter's own aggregate function. The input loop yields to the
//! runtime every few batches (`coop`).

use std::collections::HashMap;

use arrow_array::{Array, ArrayRef, BooleanArray, RecordBatch};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use interlaken_ir::interp::{Datum, RowKey};
use interlaken_ir::{Expr, Type};

use super::accumulate::{Accumulator, Arg, Kind};
use super::pull::{Pull, batch, into_stream, row_keys};
use crate::columnar::{build_array, datum_at};
use crate::coop::Yielder;
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over, execute};
use crate::kernels::CompiledExpr;
use crate::memory::{Reservation, key_bytes, row_heap_bytes, table_capacity_bytes, vec_bytes};
use crate::physical::{Aggregate, BATCH_ROWS, Physical};

/// One aggregate, compiled.
struct Compiled {
    /// Its definition and flags.
    agg: Aggregate,
    /// How it accumulates.
    kind: Kind,
    /// The compiled arguments.
    args: Vec<CompiledExpr>,
    /// The compiled filter.
    filter: Option<CompiledExpr>,
}

/// An aggregation in progress.
struct AggregateState {
    /// The input, until it is consumed.
    input: Option<BatchStream>,
    /// The compiled grouping expressions.
    keys: Vec<CompiledExpr>,
    /// The compiled aggregates.
    aggs: Vec<Compiled>,
    /// The output types, keys first.
    types: Vec<Type>,
    /// The output schema.
    schema: SchemaRef,
    /// Every group's key values, in first-seen order, once the input is consumed.
    groups: Vec<Vec<Datum>>,
    /// Every group's accumulators, one per aggregate, group after group.
    states: Vec<Accumulator>,
    /// The groups already returned.
    emitted: usize,
    /// The memory held by groups and their accumulators.
    memory: Reservation,
}

/// Return the rows of a predicate result that are TRUE.
fn true_rows(result: &ArrayRef) -> Result<Vec<usize>, ExecError> {
    let result = result
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| ExecError::Unsupported("non-boolean aggregate filter".into()))?;
    Ok((0..result.len())
        .filter(|&i| result.is_valid(i) && result.value(i))
        .collect())
}

/// Grow `memory` by a signed change in heap bytes.
fn apply_delta(memory: &mut Reservation, delta: isize) -> Result<(), ExecError> {
    match usize::try_from(delta) {
        Ok(grown) => memory.grow(grown),
        Err(_) => {
            memory.shrink(delta.unsigned_abs());
            Ok(())
        }
    }
}

impl AggregateState {
    /// Consume the input, folding every row into its group's accumulators.
    ///
    /// Each group's key is held once, in the table, and its accumulators
    /// in one vector shared by all groups; at the end the keys move out of
    /// the table into first-seen order.
    async fn run(&mut self) -> Result<(), ExecError> {
        let mut input = self.input.take().expect("run once");
        let mut groups: HashMap<RowKey, usize> = HashMap::new();
        let width = self.aggs.len();
        let fresh: Vec<Accumulator> = self.aggs.iter().map(|a| a.kind.fresh()).collect();
        let mut yielder = Yielder::default();
        while let Some(b) = input.next().await {
            let b = b?;
            yielder.tick().await;
            let n = b.num_rows();
            let keys = self
                .keys
                .iter_mut()
                .map(|k| k.eval(b.columns(), n))
                .collect::<Result<Vec<_>, _>>()?;
            let mut group_of: Vec<usize> = Vec::with_capacity(n);
            for key in row_keys(&keys, n)? {
                let group = match groups.get(&key) {
                    Some(&g) => g,
                    None => {
                        let g = groups.len();
                        let states = &mut self.states;
                        let grown = if states.len() + width > states.capacity() {
                            let wanted = (2 * states.capacity()).max(states.len() + width);
                            vec_bytes::<Accumulator>(wanted)
                                - vec_bytes::<Accumulator>(states.capacity())
                        } else {
                            0
                        };
                        let replaced = self.memory.reserve_entry::<(RowKey, usize)>(
                            groups.len(),
                            groups.capacity(),
                            key_bytes(&key) + grown,
                        )?;
                        states.extend_from_slice(&fresh);
                        groups.insert(key, g);
                        self.memory.shrink(replaced);
                        g
                    }
                };
                group_of.push(group);
            }
            for (a, compiled) in self.aggs.iter_mut().enumerate() {
                // Arguments are evaluated only for the rows the filter keeps.
                let rows: Vec<usize> = match &mut compiled.filter {
                    Some(filter) => true_rows(&filter.eval(b.columns(), n)?)?,
                    None => (0..n).collect(),
                };
                let selected = if rows.len() == n {
                    b.clone()
                } else {
                    let mask: BooleanArray = (0..n)
                        .map(|i| Some(rows.binary_search(&i).is_ok()))
                        .collect();
                    arrow_select::filter::filter_record_batch(&b, &mask)?
                };
                let args = compiled
                    .args
                    .iter_mut()
                    .map(|e| e.eval(selected.columns(), selected.num_rows()))
                    .collect::<Result<Vec<_>, _>>()?;
                let strict = compiled.agg.def.properties.strict;
                if compiled.kind == Kind::Tuples {
                    for (k, &row) in rows.iter().enumerate() {
                        let mut tuple = Vec::with_capacity(args.len());
                        for c in &args {
                            tuple.push(datum_at(c.as_ref(), k)?);
                        }
                        let bytes = self.states[group_of[row] * width + a].push_tuple(tuple)?;
                        self.memory.grow(bytes)?;
                    }
                    continue;
                }
                let arg = Arg::of(&args);
                for (k, &row) in rows.iter().enumerate() {
                    let delta = self.states[group_of[row] * width + a].update(
                        compiled.kind,
                        &arg,
                        k,
                        strict,
                    )?;
                    apply_delta(&mut self.memory, delta)?;
                }
            }
        }
        if self.keys.is_empty() && groups.is_empty() {
            self.states = fresh;
            self.groups.push(Vec::new());
            return Ok(());
        }
        // Move the keys out of the table, in first-seen order.
        let table = table_capacity_bytes::<(RowKey, usize)>(groups.capacity());
        self.memory.grow(vec_bytes::<Vec<Datum>>(groups.len()))?;
        let mut ordered: Vec<Vec<Datum>> = vec![Vec::new(); groups.len()];
        for (key, g) in groups {
            ordered[g] = key.0;
        }
        self.memory.shrink(table);
        self.groups = ordered;
        Ok(())
    }

    /// Finish the next batch of groups, moving their keys and accumulators out.
    fn emit(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        let start = self.emitted;
        let end = (start + BATCH_ROWS).min(self.groups.len());
        if start == end {
            return Ok(None);
        }
        self.emitted = end;
        let width = self.aggs.len();
        let nkeys = self.keys.len();
        let mut columns: Vec<Vec<Datum>> = (0..nkeys + width)
            .map(|_| Vec::with_capacity(end - start))
            .collect();
        let mut released = 0;
        for g in start..end {
            let key = std::mem::take(&mut self.groups[g]);
            released += row_heap_bytes(&key);
            for (c, value) in key.into_iter().enumerate() {
                columns[c].push(value);
            }
            for (a, compiled) in self.aggs.iter().enumerate() {
                let state =
                    std::mem::replace(&mut self.states[g * width + a], Accumulator::Count(0));
                released += state.heap_bytes();
                columns[nkeys + a].push(state.finish(&compiled.agg, compiled.kind)?);
            }
        }
        self.memory.shrink(released);
        let arrays = self
            .types
            .iter()
            .zip(&columns)
            .map(|(ty, values)| build_array(&ty.data, values))
            .collect::<Result<Vec<_>, _>>()?;
        batch(&self.schema, arrays, end - start).map(Some)
    }
}

impl Pull for AggregateState {
    /// Aggregate everything on the first pull, then return the groups a batch at a time.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if self.input.is_some() {
            self.run().await?;
        }
        self.emit()
    }
}

/// Start aggregating `input` by `keys`, producing `node`'s schema.
pub(crate) fn aggregate(
    node: &Physical,
    input: &Physical,
    keys: &[Expr],
    aggs: &[Aggregate],
    ctx: &ExecContext,
) -> Result<BatchStream, ExecError> {
    let compile = |e: &Expr| compile_over(e, &input.layout, &input.types, ctx);
    let keys = keys.iter().map(compile).collect::<Result<_, _>>()?;
    let aggs = aggs
        .iter()
        .map(|a| {
            Ok(Compiled {
                agg: a.clone(),
                kind: Kind::of(a),
                args: a
                    .args
                    .iter()
                    .map(compile)
                    .collect::<Result<_, ExecError>>()?,
                filter: a.filter.as_ref().map(compile).transpose()?,
            })
        })
        .collect::<Result<_, ExecError>>()?;
    Ok(into_stream(AggregateState {
        input: Some(execute(input, ctx)),
        keys,
        aggs,
        types: node.types.clone(),
        schema: node.schema.clone(),
        groups: Vec::new(),
        states: Vec::new(),
        emitted: 0,
        memory: ctx.reserve(node, "HashAggregate"),
    }))
}
