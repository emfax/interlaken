//! `Project`: compute the output columns of each batch.

use arrow_array::{RecordBatch, RecordBatchOptions};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use futures::future::ready;
use futures::stream::TryStreamExt;

use crate::exec::BatchStream;
use crate::kernels::CompiledExpr;

/// Project each batch of `input` through compiled expressions into `schema`.
pub(crate) fn project(
    input: BatchStream,
    mut exprs: Vec<CompiledExpr>,
    schema: SchemaRef,
) -> BatchStream {
    input
        .and_then(move |batch| {
            let result = exprs
                .iter_mut()
                .map(|e| e.eval(batch.columns(), batch.num_rows()))
                .collect::<Result<Vec<_>, _>>()
                .and_then(|columns| {
                    let options = RecordBatchOptions::new().with_row_count(Some(batch.num_rows()));
                    Ok(RecordBatch::try_new_with_options(
                        schema.clone(),
                        columns,
                        &options,
                    )?)
                });
            ready(result)
        })
        .boxed()
}
