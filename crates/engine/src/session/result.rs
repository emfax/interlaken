//! What a query returns.

use arrow_array::RecordBatch;
use interlaken_exec::datum_at;
use interlaken_ir::interp::Datum;
use interlaken_ir::{DataType, ScalarType, Type};

use super::usage::Usage;

/// The rows a query (or a DML statement's `RETURNING`) produced.
#[derive(Clone, Debug)]
pub struct QueryResult {
    /// The output column names; unnamed columns are `?column?`, as in Postgres.
    pub columns: Vec<String>,
    /// The output columns' IR types.
    pub types: Vec<Type>,
    /// The rows, as Arrow batches.
    pub batches: Vec<RecordBatch>,
    /// What the statement cost (ADR-0039).
    pub usage: Usage,
    /// The commit timestamp of a statement that ran in its own (implicit)
    /// transaction and wrote something (ADR-0040); `None` otherwise.
    pub commit_ts: Option<crate::CommitTs>,
}

impl QueryResult {
    /// Return the number of rows.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }

    /// Return the rows as values, in order, typed by [`QueryResult::types`].
    ///
    /// # Panics
    ///
    /// Panics if a batch holds an Arrow type the executor never produces.
    #[must_use]
    pub fn rows(&self) -> Vec<Vec<Datum>> {
        let mut out = Vec::with_capacity(self.row_count());
        for batch in &self.batches {
            for row in 0..batch.num_rows() {
                out.push(
                    batch
                        .columns()
                        .iter()
                        .zip(&self.types)
                        .map(|(c, ty)| {
                            typed(
                                datum_at(c.as_ref(), row).expect("executor output type"),
                                &ty.data,
                            )
                        })
                        .collect(),
                );
            }
        }
        out
    }
}

/// Return `value` as a value of `ty` where Arrow lost the distinction:
/// Arrow carries `jsonb` as text.
fn typed(value: Datum, ty: &DataType) -> Datum {
    match (value, ty) {
        (Datum::Text(s), DataType::Scalar(ScalarType::Jsonb)) => Datum::Jsonb(s),
        (Datum::Array(items), DataType::Array(element)) => Datum::Array(
            items
                .into_iter()
                .map(|item| typed(item, &element.data))
                .collect(),
        ),
        (Datum::Record(fields), DataType::Record(types)) => Datum::Record(
            fields
                .into_iter()
                .zip(types)
                .map(|(field, f)| typed(field, &f.ty.data))
                .collect(),
        ),
        (value, _) => value,
    }
}
