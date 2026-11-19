//! `WriteCheck`: authorize each batch of DML input before it is written (ADR-0025).

use arrow_array::{Array, BooleanArray};
use futures::StreamExt;
use futures::future::ready;
use futures::stream::TryStreamExt;

use crate::error::ExecError;
use crate::exec::BatchStream;
use crate::kernels::CompiledExpr;

/// Pass `input` through, failing with a permission error naming `table` at
/// the first batch with a row whose check is not TRUE.
///
/// The batch is checked before the DML operator above sees any of it, so a
/// rejected row is never written; the error carries no row values.
pub(crate) fn write_check(
    input: BatchStream,
    mut check: CompiledExpr,
    table: String,
) -> BatchStream {
    input
        .and_then(move |batch| {
            let result = check
                .eval(batch.columns(), batch.num_rows())
                .and_then(|passed| {
                    let passed = passed
                        .as_any()
                        .downcast_ref::<BooleanArray>()
                        .ok_or_else(|| ExecError::Unsupported("non-boolean write check".into()))?;
                    let all = passed.null_count() == 0 && passed.true_count() == passed.len();
                    if all {
                        Ok(batch)
                    } else {
                        Err(ExecError::PermissionDenied {
                            table: table.clone(),
                        })
                    }
                });
            ready(result)
        })
        .boxed()
}
