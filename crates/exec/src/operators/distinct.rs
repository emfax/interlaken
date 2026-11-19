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
use crate::memory::{Reservation, entry_bytes};

/// Return the rows of `b` not seen before (by value, NULLs equal), recording
/// them in `seen` and the memory they take in `memory`.
pub(crate) fn first_occurrences(
    b: &RecordBatch,
    seen: &mut HashSet<RowKey>,
    memory: &mut Reservation,
    schema: &SchemaRef,
) -> Result<RecordBatch, ExecError> {
    let mut keep = Vec::new();
    for (i, key) in row_keys(b.columns(), b.num_rows())?.into_iter().enumerate() {
        if !seen.contains(&key) {
            memory.grow(entry_bytes(&key, 0))?;
            seen.insert(key);
            keep.push(u32::try_from(i).expect("rows fit in u32"));
        }
    }
    let keep = UInt32Array::from(keep);
    let columns = take(b.columns(), &keep)?;
    batch(schema, columns, keep.len())
}

/// Stream the first occurrence of each distinct row of `input`, holding the
/// rows seen in `memory`.
pub(crate) fn distinct(
    input: BatchStream,
    schema: SchemaRef,
    mut memory: Reservation,
) -> BatchStream {
    let mut seen = HashSet::new();
    input
        .and_then(move |b| ready(first_occurrences(&b, &mut seen, &mut memory, &schema)))
        .try_filter(|b| ready(b.num_rows() > 0))
        .boxed()
}
