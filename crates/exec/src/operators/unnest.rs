//! `Unnest`: one output row per element of an array.
//!
//! The array expression is evaluated for the whole batch. Its list offsets
//! say which input row each element belongs to, so the input columns are
//! repeated with one `take` and the elements gathered with another. A NULL
//! or empty array produces no rows. A record element contributes one
//! column per field (all NULL for a NULL record); a scalar element one.

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, BooleanArray, RecordBatch, UInt32Array};
use arrow_schema::{DataType as ArrowType, SchemaRef};
use futures::future::ready;
use futures::stream::{self, StreamExt, TryStreamExt};

use super::pull::{batch, chunks, take};
use crate::error::ExecError;
use crate::exec::BatchStream;
use crate::kernels::CompiledExpr;

/// Return the element columns of `elements`: a record's fields, or the elements themselves.
fn element_columns(elements: &ArrayRef, width: usize) -> Result<Vec<ArrayRef>, ExecError> {
    let ArrowType::Struct(_) = elements.data_type() else {
        return Ok(vec![elements.clone()]);
    };
    let records = elements.as_struct();
    if records.num_columns() != width {
        return Err(ExecError::Unsupported(format!(
            "unnesting records of {} fields into {width} columns",
            records.num_columns()
        )));
    }
    // A NULL record's fields may hold anything in Arrow; they read as NULL.
    let null_records: Option<BooleanArray> = (records.null_count() > 0).then(|| {
        (0..records.len())
            .map(|i| Some(records.is_null(i)))
            .collect()
    });
    records
        .columns()
        .iter()
        .map(|c| match &null_records {
            Some(mask) => Ok(arrow_select::nullif::nullif(c.as_ref(), mask)?),
            None => Ok(c.clone()),
        })
        .collect()
}

/// Expand one batch: each input row once per element of its array.
fn expand(
    b: &RecordBatch,
    expr: &mut CompiledExpr,
    width: usize,
    schema: &SchemaRef,
) -> Result<Vec<RecordBatch>, ExecError> {
    let arrays = expr.eval(b.columns(), b.num_rows())?;
    let list = arrays
        .as_list_opt::<i32>()
        .ok_or_else(|| ExecError::Unsupported("unnesting a non-array".into()))?;
    let offsets = list.value_offsets();
    let (mut rows, mut elements) = (Vec::new(), Vec::new());
    for i in 0..list.len() {
        if list.is_null(i) {
            continue;
        }
        let row = u32::try_from(i).expect("rows fit in u32");
        for e in offsets[i]..offsets[i + 1] {
            rows.push(row);
            elements.push(u32::try_from(e).expect("non-negative offsets"));
        }
    }
    let (rows, elements) = (UInt32Array::from(rows), UInt32Array::from(elements));
    let mut columns = take(b.columns(), &rows)?;
    let taken = arrow_select::take::take(list.values().as_ref(), &elements, None)?;
    columns.extend(element_columns(&taken, width)?);
    Ok(chunks(&batch(schema, columns, rows.len())?))
}

/// Unnest `expr`, evaluated over each batch of `input`, into `width` element columns.
pub(crate) fn unnest(
    input: BatchStream,
    mut expr: CompiledExpr,
    width: usize,
    schema: SchemaRef,
) -> BatchStream {
    input
        .and_then(move |b| ready(expand(&b, &mut expr, width, &schema)))
        .map_ok(|batches| stream::iter(batches.into_iter().map(Ok)))
        .try_flatten()
        .try_filter(|b| ready(b.num_rows() > 0))
        .boxed()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Int32Array, StructArray};
    use arrow_buffer::NullBuffer;
    use arrow_schema::{Field, Fields};

    use super::*;

    #[test]
    /// Assert a NULL record's fields read as NULL even when Arrow holds values under it.
    fn null_records_have_null_fields() {
        let fields = Fields::from(vec![Field::new("a", ArrowType::Int32, true)]);
        let values: ArrayRef = Arc::new(Int32Array::from(vec![1, 2]));
        let records: ArrayRef = Arc::new(
            StructArray::try_new(
                fields,
                vec![values],
                Some(NullBuffer::from(vec![true, false])),
            )
            .unwrap(),
        );
        let columns = element_columns(&records, 1).unwrap();
        assert_eq!(
            columns[0].as_ref(),
            &Int32Array::from(vec![Some(1), None]) as &dyn Array
        );
        assert!(element_columns(&records, 2).is_err());
    }
}
