//! `Project`: compute the output columns of each row.

use arrow_array::RecordBatch;
use futures::StreamExt;
use futures::future::ready;
use futures::stream::TryStreamExt;
use interlaken_ir::Expr;

use crate::exec::{BatchStream, ExecContext};
use crate::physical::{Layout, Physical};
use crate::rowwise::{eval_column, rows_of};

/// Project each batch of `input` through `exprs` into `node`'s schema.
pub(crate) fn project(
    input: BatchStream,
    exprs: Vec<Expr>,
    input_layout: Layout,
    node: &Physical,
    ctx: &ExecContext,
) -> BatchStream {
    let (ctx, schema, types) = (ctx.clone(), node.schema.clone(), node.types.clone());
    input
        .and_then(move |batch| {
            let result = rows_of(&batch).and_then(|rows| {
                let columns = exprs
                    .iter()
                    .zip(&types)
                    .map(|(e, ty)| eval_column(&ctx, e, &input_layout, &rows, &ty.data))
                    .collect::<Result<Vec<_>, _>>()?;
                let options =
                    arrow_array::RecordBatchOptions::new().with_row_count(Some(batch.num_rows()));
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
