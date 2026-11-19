//! `Limit`: skip and cap rows, and stop pulling once satisfied.

use futures::stream::{self, StreamExt};
use interlaken_ir::Expr;
use interlaken_ir::interp::{Datum, EvalError};

use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext};
use crate::rowwise::eval_constant;

/// Evaluate a limit or offset: `None` when absent or NULL; negative is an error.
fn count(
    ctx: &ExecContext,
    expr: Option<&Expr>,
    what: &'static str,
) -> Result<Option<usize>, ExecError> {
    let Some(expr) = expr else { return Ok(None) };
    match eval_constant(ctx, expr)? {
        Datum::Null => Ok(None),
        Datum::Int8(n) if n < 0 => Err(EvalError::NegativeLimit(what).into()),
        Datum::Int8(n) => Ok(Some(usize::try_from(n).unwrap_or(usize::MAX))),
        other => Err(ExecError::Unsupported(format!("{what} of {other:?}"))),
    }
}

/// Where a limit is.
struct State {
    /// The input; dropped, cancelling it, once the limit is reached or it fails.
    input: Option<BatchStream>,
    /// Rows still to skip.
    skip: usize,
    /// Rows still to return.
    take: usize,
}

/// Apply `limit` and `offset` to `input`.
pub(crate) fn limit(
    input: BatchStream,
    limit: Option<Expr>,
    offset: Option<Expr>,
    ctx: &ExecContext,
) -> BatchStream {
    let counts = count(ctx, limit.as_ref(), "LIMIT")
        .and_then(|l| Ok((l, count(ctx, offset.as_ref(), "OFFSET")?)));
    let (take, skip) = match counts {
        Ok((l, o)) => (l.unwrap_or(usize::MAX), o.unwrap_or(0)),
        Err(e) => return stream::once(async move { Err(e) }).boxed(),
    };
    let state = State {
        input: Some(input),
        skip,
        take,
    };
    stream::unfold(state, |mut state| async move {
        loop {
            if state.take == 0 {
                // Ending the stream drops the state and with it the input, cancelling it.
                return None;
            }
            let batch = match state.input.as_mut()?.next().await? {
                Ok(batch) => batch,
                Err(e) => {
                    return Some((
                        Err(e),
                        State {
                            input: None,
                            ..state
                        },
                    ));
                }
            };
            let rows = batch.num_rows();
            if state.skip >= rows {
                state.skip -= rows;
                continue;
            }
            let start = state.skip;
            let len = (rows - start).min(state.take);
            state.skip = 0;
            state.take -= len;
            return Some((Ok(batch.slice(start, len)), state));
        }
    })
    .boxed()
}
