//! `UNION`, `INTERSECT` and `EXCEPT`, with and without `ALL`.
//!
//! The right input of `INTERSECT` and `EXCEPT` is counted by row value;
//! left rows then consume or are removed by those counts. The distinct
//! forms keep the first occurrence of each result row.

use std::collections::HashMap;

use arrow_array::{RecordBatch, UInt32Array};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use futures::future::ready;
use futures::stream::{self, TryStreamExt};
use interlaken_ir::SetOpKind;
use interlaken_ir::interp::RowKey;

use super::distinct::distinct;
use super::pull::{Pull, batch, into_stream, row_keys, take};
use crate::coop::Yielder;
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, execute};
use crate::memory::{Reservation, entry_bytes};
use crate::physical::Physical;

/// Relabel a batch with the set operation's schema.
fn relabel(b: RecordBatch, schema: &SchemaRef) -> Result<RecordBatch, ExecError> {
    batch(schema, b.columns().to_vec(), b.num_rows())
}

/// An `INTERSECT` or `EXCEPT` in progress.
struct Filtered {
    /// The operation.
    op: SetOpKind,
    /// The left input.
    left: BatchStream,
    /// The right input, until it is counted.
    right: Option<BatchStream>,
    /// Remaining counts of right rows by value.
    counts: HashMap<RowKey, usize>,
    /// Whether duplicates are kept (`ALL`).
    all: bool,
    /// The output schema.
    schema: SchemaRef,
    /// The memory held by the right input's counts.
    memory: Reservation,
}

impl Pull for Filtered {
    /// Count the right input on the first pull, then filter one left batch per pull.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if let Some(mut right) = self.right.take() {
            let mut yielder = Yielder::default();
            while let Some(b) = right.next().await {
                let b = b?;
                yielder.tick().await;
                for key in row_keys(b.columns(), b.num_rows())? {
                    match self.counts.get_mut(&key) {
                        Some(n) => *n += 1,
                        None => {
                            self.memory
                                .grow(entry_bytes(&key, std::mem::size_of::<usize>()))?;
                            self.counts.insert(key, 1);
                        }
                    }
                }
            }
        }
        let Some(b) = self.left.next().await else {
            return Ok(None);
        };
        let b = b?;
        let keep: UInt32Array = row_keys(b.columns(), b.num_rows())?
            .into_iter()
            .enumerate()
            .filter(|(_, key)| {
                let available = self.counts.get_mut(key);
                match (self.op, self.all) {
                    // A left row needs (INTERSECT) or is cancelled by (EXCEPT ALL) one right row.
                    (SetOpKind::Intersect, _) | (SetOpKind::Except, true) => {
                        let consumed = match available {
                            Some(n) if *n > 0 => {
                                *n -= 1;
                                true
                            }
                            _ => false,
                        };
                        consumed == (self.op == SetOpKind::Intersect)
                    }
                    _ => available.is_none(),
                }
            })
            .map(|(i, _)| u32::try_from(i).expect("rows fit in u32"))
            .collect();
        let columns = take(b.columns(), &keep)?;
        batch(&self.schema, columns, keep.len()).map(Some)
    }
}

/// Start a set operation of `left` and `right`, producing `node`'s schema.
pub(crate) fn set_op(
    node: &Physical,
    op: SetOpKind,
    all: bool,
    left: &Physical,
    right: &Physical,
    ctx: &ExecContext,
) -> BatchStream {
    let schema = node.schema.clone();
    let combined = match op {
        SetOpKind::Union => {
            let s = schema.clone();
            stream::iter([execute(left, ctx), execute(right, ctx)])
                .flatten()
                .and_then(move |b| ready(relabel(b, &s)))
                .boxed()
        }
        SetOpKind::Intersect | SetOpKind::Except => into_stream(Filtered {
            op,
            left: execute(left, ctx),
            right: Some(execute(right, ctx)),
            counts: HashMap::new(),
            all,
            schema: schema.clone(),
            memory: ctx.reserve(node, "SetOp"),
        }),
    };
    if all {
        combined
    } else {
        distinct(combined, schema, ctx.reserve(node, "SetOp"))
    }
}
