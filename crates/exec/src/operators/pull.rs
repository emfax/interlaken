//! Operators written as state machines that produce one batch per pull.

use std::future::Future;

use arrow_array::{ArrayRef, RecordBatch, RecordBatchOptions};
use arrow_schema::SchemaRef;
use futures::stream::{self, StreamExt, TryStreamExt};
use interlaken_ir::interp::{Datum, RowKey};

use crate::columnar::datum_at;
use crate::error::ExecError;
use crate::exec::BatchStream;
use crate::physical::BATCH_ROWS;

/// An operator that produces its output one batch at a time.
pub(crate) trait Pull: Send + 'static {
    /// Produce the next batch, or `None` when the operator is finished.
    fn next(&mut self) -> impl Future<Output = Result<Option<RecordBatch>, ExecError>> + Send;
}

/// Turn an operator into a stream of its batches, dropping empty ones.
pub(crate) fn into_stream<P: Pull>(operator: P) -> BatchStream {
    stream::try_unfold(operator, |mut operator| async move {
        Ok(operator.next().await?.map(|b| (b, operator)))
    })
    .try_filter(|batch| futures::future::ready(batch.num_rows() > 0))
    .boxed()
}

/// Concatenate batches into one with `schema`.
pub(crate) fn concat(
    schema: &SchemaRef,
    batches: &[RecordBatch],
) -> Result<RecordBatch, ExecError> {
    Ok(arrow_select::concat::concat_batches(schema, batches)?)
}

/// Build a batch with `schema` from columns (which may have no columns but some rows).
pub(crate) fn batch(
    schema: &SchemaRef,
    columns: Vec<ArrayRef>,
    rows: usize,
) -> Result<RecordBatch, ExecError> {
    let options = RecordBatchOptions::new().with_row_count(Some(rows));
    Ok(RecordBatch::try_new_with_options(
        schema.clone(),
        columns,
        &options,
    )?)
}

/// Split a batch into batches of at most [`BATCH_ROWS`] rows.
pub(crate) fn chunks(batch: &RecordBatch) -> Vec<RecordBatch> {
    (0..batch.num_rows())
        .step_by(BATCH_ROWS)
        .map(|start| batch.slice(start, BATCH_ROWS.min(batch.num_rows() - start)))
        .collect()
}

/// Return the values of each row of `columns` as a hashable key.
///
/// Each key's vector holds exactly its values (collecting through a
/// `Result` would leave room for at least four), since hash tables keep
/// millions of them.
pub(crate) fn row_keys(columns: &[ArrayRef], rows: usize) -> Result<Vec<RowKey>, ExecError> {
    (0..rows)
        .map(|r| {
            let mut values: Vec<Datum> = Vec::with_capacity(columns.len());
            for c in columns {
                values.push(datum_at(c.as_ref(), r)?);
            }
            Ok(RowKey(values))
        })
        .collect()
}

/// Take the rows at `indices` of every column; a NULL index makes an all-NULL row.
pub(crate) fn take(
    columns: &[ArrayRef],
    indices: &arrow_array::UInt32Array,
) -> Result<Vec<ArrayRef>, ExecError> {
    columns
        .iter()
        .map(|c| Ok(arrow_select::take::take(c.as_ref(), indices, None)?))
        .collect()
}
