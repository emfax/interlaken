//! `Nest`: group rows and collect each group's fields into a list of records (ADR-0012).
//!
//! Keys and fields are evaluated batch by batch and kept as Arrow arrays;
//! rows are assigned to groups in first-seen order. At the end, one `take`
//! gathers each group's first key values and another gathers every
//! group's records, contiguously, as the child of a list-of-struct column
//! whose offsets mark the groups. Records keep their input order within a
//! group, so a sort below orders the nested lists. Without keys the output
//! is exactly one row, whose list is empty when the input is.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::{ArrayRef, ListArray, RecordBatch, StructArray, UInt32Array, new_empty_array};
use arrow_buffer::{OffsetBuffer, ScalarBuffer};
use arrow_schema::{DataType as ArrowType, SchemaRef};
use futures::StreamExt;
use interlaken_ir::Expr;
use interlaken_ir::interp::RowKey;

use super::pull::{Pull, batch, chunks, into_stream, row_keys};
use crate::coop::Yielder;
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over, execute};
use crate::kernels::CompiledExpr;
use crate::memory::{Reservation, entry_bytes};
use crate::physical::Physical;

/// A nest in progress.
struct NestState {
    /// The input, until it is consumed.
    input: Option<BatchStream>,
    /// The compiled grouping expressions.
    keys: Vec<CompiledExpr>,
    /// The compiled record fields.
    fields: Vec<CompiledExpr>,
    /// The output schema: the keys, then the list of records.
    schema: SchemaRef,
    /// The memory held for evaluated rows and groups.
    memory: Reservation,
    /// Output batches not yet returned, last first.
    ready: Vec<RecordBatch>,
}

/// Concatenate the arrays of one column, or build an empty one of `ty`.
fn concat_column(parts: &[ArrayRef], ty: &ArrowType) -> Result<ArrayRef, ExecError> {
    if parts.is_empty() {
        return Ok(new_empty_array(ty));
    }
    let refs: Vec<&dyn arrow_array::Array> = parts.iter().map(AsRef::as_ref).collect();
    Ok(arrow_select::concat::concat(&refs)?)
}

impl NestState {
    /// Consume the input and build every group's output row.
    async fn run(&mut self) -> Result<(), ExecError> {
        let mut input = self.input.take().expect("run once");
        let mut key_parts: Vec<Vec<ArrayRef>> = vec![Vec::new(); self.keys.len()];
        let mut field_parts: Vec<Vec<ArrayRef>> = vec![Vec::new(); self.fields.len()];
        let mut groups: HashMap<RowKey, usize> = HashMap::new();
        let mut members: Vec<Vec<u32>> = Vec::new();
        let mut seen = 0u32;
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
            let fields = self
                .fields
                .iter_mut()
                .map(|f| f.eval(b.columns(), n))
                .collect::<Result<Vec<_>, _>>()?;
            let held: usize = keys
                .iter()
                .chain(&fields)
                .map(|a| a.get_array_memory_size())
                .sum();
            self.memory.grow(held + n * std::mem::size_of::<u32>())?;
            for (r, key) in row_keys(&keys, n)?.into_iter().enumerate() {
                let next = members.len();
                let group = match groups.get(&key) {
                    Some(&g) => g,
                    None => {
                        self.memory
                            .grow(entry_bytes(&key, std::mem::size_of::<Vec<u32>>()))?;
                        groups.insert(key, next);
                        members.push(Vec::new());
                        next
                    }
                };
                members[group].push(seen + u32::try_from(r).expect("rows fit in u32"));
            }
            seen += u32::try_from(n).expect("rows fit in u32");
            for (parts, a) in key_parts.iter_mut().zip(keys) {
                parts.push(a);
            }
            for (parts, a) in field_parts.iter_mut().zip(fields) {
                parts.push(a);
            }
        }
        if self.keys.is_empty() && members.is_empty() {
            members.push(Vec::new());
        }

        let fields = self.schema.fields();
        let nested_type = fields[fields.len() - 1].data_type().clone();
        let ArrowType::List(item) = &nested_type else {
            return Err(ExecError::Unsupported(format!(
                "nesting into {nested_type}"
            )));
        };
        let ArrowType::Struct(record_fields) = item.data_type() else {
            return Err(ExecError::Unsupported(format!(
                "nesting into {nested_type}"
            )));
        };
        let firsts = UInt32Array::from(
            members
                .iter()
                .map(|m| m.first().copied())
                .collect::<Vec<Option<u32>>>(),
        );
        let mut columns = key_parts
            .iter()
            .zip(fields.iter())
            .map(|(parts, f)| {
                let all = concat_column(parts, f.data_type())?;
                Ok(arrow_select::take::take(all.as_ref(), &firsts, None)?)
            })
            .collect::<Result<Vec<ArrayRef>, ExecError>>()?;
        let order = UInt32Array::from(members.concat());
        let children = field_parts
            .iter()
            .zip(record_fields.iter())
            .map(|(parts, f)| {
                let all = concat_column(parts, f.data_type())?;
                Ok(arrow_select::take::take(all.as_ref(), &order, None)?)
            })
            .collect::<Result<Vec<ArrayRef>, ExecError>>()?;
        let records =
            StructArray::try_new_with_length(record_fields.clone(), children, None, order.len())?;
        let mut offsets = Vec::with_capacity(members.len() + 1);
        offsets.push(0i32);
        for m in &members {
            let last = *offsets.last().expect("starts with 0");
            offsets.push(last + i32::try_from(m.len()).expect("lists fit in i32"));
        }
        columns.push(Arc::new(ListArray::try_new(
            item.clone(),
            OffsetBuffer::new(ScalarBuffer::from(offsets)),
            Arc::new(records),
            None,
        )?));
        let out = batch(&self.schema, columns, members.len())?;
        self.ready = chunks(&out);
        self.ready.reverse();
        Ok(())
    }
}

impl Pull for NestState {
    /// Build the groups on the first pull, then return them a batch at a time.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if self.input.is_some() {
            self.run().await?;
        }
        Ok(self.ready.pop())
    }
}

/// Group the rows of `input` by `keys`, nesting `fields` (ADR-0012).
///
/// # Errors
///
/// Returns an error if an expression does not compile.
pub(crate) fn nest(
    node: &Physical,
    input: &Physical,
    keys: &[Expr],
    fields: &[Expr],
    ctx: &ExecContext,
) -> Result<BatchStream, ExecError> {
    let compile = |e: &Expr| compile_over(e, &input.layout, &input.types, ctx);
    Ok(into_stream(NestState {
        input: Some(execute(input, ctx)),
        keys: keys.iter().map(compile).collect::<Result<_, _>>()?,
        fields: fields.iter().map(compile).collect::<Result<_, _>>()?,
        schema: node.schema.clone(),
        memory: ctx.reserve(node, "Nest"),
        ready: Vec::new(),
    }))
}
