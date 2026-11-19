//! `Aggregate`: hash aggregation.
//!
//! Group keys are evaluated for every row; each aggregate's arguments only
//! for the rows its `FILTER` keeps, as in the interpreter. Plain `count`
//! is counted as rows arrive; every other aggregate keeps its argument
//! tuples and is finished by the interpreter's own aggregate function, so
//! results (including float summation order within a group) match it.

use std::collections::HashMap;

use arrow_array::{Array, ArrayRef, BooleanArray, RecordBatch};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use interlaken_ir::interp::{Datum, RowKey, aggregate_values};
use interlaken_ir::{Expr, Type};

use super::pull::{Pull, batch, chunks, into_stream, row_keys};
use crate::columnar::{build_array, datum_at};
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over, execute};
use crate::kernels::CompiledExpr;
use crate::physical::{Aggregate, Physical};

/// One aggregate's state for one group.
#[derive(Clone, Debug)]
enum Accumulator {
    /// A non-distinct `count`: the rows counted so far.
    Count(i64),
    /// The argument tuples of the rows seen so far.
    Tuples(Vec<Vec<Datum>>),
}

/// One aggregate, compiled.
struct Compiled {
    /// Its definition and flags.
    agg: Aggregate,
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
    /// Output batches not yet returned.
    ready: Vec<RecordBatch>,
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

impl AggregateState {
    /// Consume the input and compute every group's output.
    async fn run(&mut self) -> Result<(), ExecError> {
        let mut input = self.input.take().expect("run once");
        let mut groups: HashMap<RowKey, usize> = HashMap::new();
        let mut order: Vec<(Vec<Datum>, Vec<Accumulator>)> = Vec::new();
        let fresh: Vec<Accumulator> = self
            .aggs
            .iter()
            .map(|a| {
                if a.agg.def.name.to_string() == "count" && !a.agg.distinct {
                    Accumulator::Count(0)
                } else {
                    Accumulator::Tuples(Vec::new())
                }
            })
            .collect();
        while let Some(b) = input.next().await {
            let b = b?;
            let n = b.num_rows();
            let keys = self
                .keys
                .iter_mut()
                .map(|k| k.eval(b.columns(), n))
                .collect::<Result<Vec<_>, _>>()?;
            let group_of: Vec<usize> = row_keys(&keys, n)?
                .into_iter()
                .map(|key| {
                    *groups.entry(key.clone()).or_insert_with(|| {
                        order.push((key.0, fresh.clone()));
                        order.len() - 1
                    })
                })
                .collect();
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
                for (k, &row) in rows.iter().enumerate() {
                    let tuple = args
                        .iter()
                        .map(|c| datum_at(c.as_ref(), k))
                        .collect::<Result<Vec<_>, _>>()?;
                    match &mut order[group_of[row]].1[a] {
                        Accumulator::Count(n) => {
                            if !(strict && tuple.iter().any(Datum::is_null)) {
                                *n += 1;
                            }
                        }
                        Accumulator::Tuples(tuples) => tuples.push(tuple),
                    }
                }
            }
        }
        if self.keys.is_empty() && order.is_empty() {
            order.push((Vec::new(), fresh));
        }
        let mut rows: Vec<Vec<Datum>> = Vec::with_capacity(order.len());
        for (key, accumulators) in order {
            let mut row = key;
            for (accumulator, compiled) in accumulators.into_iter().zip(&self.aggs) {
                row.push(match accumulator {
                    Accumulator::Count(n) => Datum::Int8(n),
                    Accumulator::Tuples(tuples) => {
                        aggregate_values(&compiled.agg.def, tuples, compiled.agg.distinct)?
                    }
                });
            }
            rows.push(row);
        }
        let columns = self
            .types
            .iter()
            .enumerate()
            .map(|(c, ty)| {
                build_array(
                    &ty.data,
                    &rows.iter().map(|r| r[c].clone()).collect::<Vec<_>>(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let all = batch(&self.schema, columns, rows.len())?;
        self.ready = chunks(&all);
        self.ready.reverse();
        Ok(())
    }
}

impl Pull for AggregateState {
    /// Aggregate everything on the first pull, then return the groups in batches.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if self.input.is_some() {
            self.run().await?;
        }
        Ok(self.ready.pop())
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
        ready: Vec::new(),
    }))
}
