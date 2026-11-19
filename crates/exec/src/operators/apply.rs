//! `Apply`: a correlated nested loop running the inner plan once per outer row.

use std::collections::VecDeque;

use arrow_array::{ArrayRef, RecordBatch, UInt32Array, new_null_array};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use interlaken_ir::ApplyKind;
use interlaken_ir::interp::EvalError;

use super::pull::{Pull, batch, into_stream, take};
use crate::columnar::datum_at;
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, OuterColumn, execute};
use crate::physical::Physical;

/// An apply in progress.
struct ApplyState {
    /// The apply kind.
    kind: ApplyKind,
    /// The outer input.
    outer: BatchStream,
    /// The outer plan's output.
    outer_node: Physical,
    /// The inner plan.
    inner: Physical,
    /// The enclosing context.
    ctx: ExecContext,
    /// The output schema.
    schema: SchemaRef,
    /// Output batches not yet returned.
    ready: VecDeque<RecordBatch>,
}

impl ApplyState {
    /// Return the context in which the inner plan runs for row `r` of `batch`.
    fn context_for(&self, batch: &RecordBatch, r: usize) -> Result<ExecContext, ExecError> {
        let row = self
            .outer_node
            .layout
            .iter()
            .zip(&self.outer_node.types)
            .zip(batch.columns())
            .map(|(((binding, column), ty), array)| {
                Ok(OuterColumn {
                    binding: *binding,
                    column: *column,
                    ty: ty.clone(),
                    value: datum_at(array.as_ref(), r)?,
                })
            })
            .collect::<Result<Vec<_>, ExecError>>()?;
        Ok(self.ctx.correlated(row))
    }

    /// Run the inner plan for every row of an outer batch, queueing the output.
    async fn process(&mut self, outer: &RecordBatch) -> Result<(), ExecError> {
        let mut kept = Vec::new();
        for r in 0..outer.num_rows() {
            let row = u32::try_from(r).expect("rows fit in u32");
            let mut inner = execute(&self.inner, &self.context_for(outer, r)?);
            match self.kind {
                ApplyKind::Semi | ApplyKind::Anti => {
                    // The first inner row decides; the rest are never computed.
                    let mut found = false;
                    while let Some(b) = inner.next().await {
                        if b?.num_rows() > 0 {
                            found = true;
                            break;
                        }
                    }
                    if found == (self.kind == ApplyKind::Semi) {
                        kept.push(row);
                    }
                }
                ApplyKind::Inner | ApplyKind::Left | ApplyKind::Single => {
                    let mut produced = 0;
                    while let Some(b) = inner.next().await {
                        let b = b?;
                        produced += b.num_rows();
                        if self.kind == ApplyKind::Single && produced > 1 {
                            return Err(EvalError::CardinalityViolation.into());
                        }
                        if b.num_rows() > 0 {
                            let repeat: UInt32Array =
                                std::iter::repeat_n(row, b.num_rows()).collect();
                            let mut columns = take(outer.columns(), &repeat)?;
                            columns.extend(b.columns().iter().cloned());
                            self.ready
                                .push_back(batch(&self.schema, columns, b.num_rows())?);
                        }
                    }
                    if produced == 0 && self.kind != ApplyKind::Inner {
                        let mut columns = take(outer.columns(), &UInt32Array::from(vec![row]))?;
                        let padding: Vec<ArrayRef> = self
                            .inner
                            .schema
                            .fields()
                            .iter()
                            .map(|f| new_null_array(f.data_type(), 1))
                            .collect();
                        columns.extend(padding);
                        self.ready.push_back(batch(&self.schema, columns, 1)?);
                    }
                }
            }
        }
        if matches!(self.kind, ApplyKind::Semi | ApplyKind::Anti) {
            let columns = take(outer.columns(), &UInt32Array::from(kept.clone()))?;
            self.ready
                .push_back(batch(&self.schema, columns, kept.len())?);
        }
        Ok(())
    }
}

impl Pull for ApplyState {
    /// Return queued output, processing outer batches as needed.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        loop {
            if let Some(b) = self.ready.pop_front() {
                return Ok(Some(b));
            }
            match self.outer.next().await {
                Some(outer) => self.process(&outer?).await?,
                None => return Ok(None),
            }
        }
    }
}

/// Start an apply of `inner` to each row of `outer`, producing `node`'s schema.
pub(crate) fn apply(
    node: &Physical,
    kind: ApplyKind,
    outer: &Physical,
    inner: &Physical,
    ctx: &ExecContext,
) -> BatchStream {
    into_stream(ApplyState {
        kind,
        outer: execute(outer, ctx),
        outer_node: outer.clone(),
        inner: inner.clone(),
        ctx: ctx.clone(),
        schema: node.schema.clone(),
        ready: VecDeque::new(),
    })
}
