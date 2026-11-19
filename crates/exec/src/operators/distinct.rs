//! `Distinct` and the duplicate elimination of set operations.

use std::collections::HashSet;

use arrow_array::{RecordBatch, UInt32Array};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use futures::future::ready;
use futures::stream::TryStreamExt;
use interlaken_ir::interp::RowKey;

use super::pull::{batch, row_keys, take};
use crate::error::ExecError;
use crate::exec::BatchStream;

/// Return the rows of `b` not seen before (by value, NULLs equal), recording them in `seen`.
pub(crate) fn first_occurrences(
    b: &RecordBatch,
    seen: &mut HashSet<RowKey>,
    schema: &SchemaRef,
) -> Result<RecordBatch, ExecError> {
    let keep: UInt32Array = row_keys(b.columns(), b.num_rows())?
        .into_iter()
        .enumerate()
        .filter(|(_, key)| seen.insert(key.clone()))
        .map(|(i, _)| u32::try_from(i).expect("rows fit in u32"))
        .collect();
    let columns = take(b.columns(), &keep)?;
    batch(schema, columns, keep.len())
}

/// Stream the first occurrence of each distinct row of `input`.
pub(crate) fn distinct(input: BatchStream, schema: SchemaRef) -> BatchStream {
    let mut seen = HashSet::new();
    input
        .and_then(move |b| ready(first_occurrences(&b, &mut seen, &schema)))
        .try_filter(|b| ready(b.num_rows() > 0))
        .boxed()
}
