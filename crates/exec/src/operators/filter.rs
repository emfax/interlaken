//! `Filter`: keep the rows where the predicate is TRUE.

use futures::StreamExt;
use futures::future::ready;
use futures::stream::TryStreamExt;
use interlaken_ir::Expr;

use crate::exec::{BatchStream, ExecContext};
use crate::physical::Layout;
use crate::rowwise::{eval_predicate, rows_of};

/// Filter `input` by `predicate`, dropping batches left empty.
pub(crate) fn filter(
    input: BatchStream,
    predicate: Expr,
    layout: Layout,
    ctx: &ExecContext,
) -> BatchStream {
    let ctx = ctx.clone();
    input
        .and_then(move |batch| {
            let result = rows_of(&batch)
                .and_then(|rows| eval_predicate(&ctx, &predicate, &layout, &rows))
                .and_then(|keep| Ok(arrow_select::filter::filter_record_batch(&batch, &keep)?));
            ready(result)
        })
        .try_filter(|batch| ready(batch.num_rows() > 0))
        .boxed()
}
