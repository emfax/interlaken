//! `Filter`: keep the rows where the predicate is TRUE.

use futures::StreamExt;
use futures::future::ready;
use futures::stream::TryStreamExt;

use crate::error::ExecError;
use crate::exec::BatchStream;
use crate::kernels::CompiledExpr;

/// Filter `input` by a compiled predicate, dropping batches left empty.
///
/// NULL and FALSE both drop a row (three-valued logic).
pub(crate) fn filter(input: BatchStream, mut predicate: CompiledExpr) -> BatchStream {
    input
        .and_then(move |batch| {
            let result = predicate
                .eval(batch.columns(), batch.num_rows())
                .and_then(|keep| {
                    let keep = keep
                        .as_any()
                        .downcast_ref::<arrow_array::BooleanArray>()
                        .ok_or_else(|| {
                            ExecError::Unsupported("non-boolean filter predicate".into())
                        })?;
                    // `filter_record_batch` drops rows where the predicate is NULL, as for FALSE.
                    Ok(arrow_select::filter::filter_record_batch(&batch, keep)?)
                });
            ready(result)
        })
        .try_filter(|batch| ready(batch.num_rows() > 0))
        .boxed()
}
