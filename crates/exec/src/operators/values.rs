//! `Values`: literal rows, possibly reading the columns of an enclosing row.

use arrow_array::{Array, RecordBatch};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use futures::stream;
use interlaken_ir::Expr;

use super::pull::batch;
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over};
use crate::physical::Physical;

/// Evaluate every row of a `Values`.
fn rows(node: &Physical, rows: &[Vec<Expr>], ctx: &ExecContext) -> Result<RecordBatch, ExecError> {
    let schema: SchemaRef = node.schema.clone();
    let columns = (0..node.layout.len())
        .map(|c| {
            let values = rows
                .iter()
                .map(|row| compile_over(&row[c], &[], &[], ctx)?.eval(&[], 1))
                .collect::<Result<Vec<_>, ExecError>>()?;
            let values: Vec<&dyn Array> = values.iter().map(AsRef::as_ref).collect();
            if values.is_empty() {
                Ok(arrow_array::new_empty_array(schema.field(c).data_type()))
            } else {
                Ok(arrow_select::concat::concat(&values)?)
            }
        })
        .collect::<Result<Vec<_>, ExecError>>()?;
    batch(&schema, columns, rows.len())
}

/// Stream the rows of a `Values` as one batch.
pub(crate) fn values(node: &Physical, exprs: &[Vec<Expr>], ctx: &ExecContext) -> BatchStream {
    let result = rows(node, exprs, ctx);
    stream::once(async move { result }).boxed()
}
