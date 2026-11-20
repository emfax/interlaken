//! `Insert`, `Update` and `Delete`: write through the row layer (ADR-0026).
//!
//! Writes go into the transaction's buffer as input batches arrive; the
//! input never sees them, because reads hide the current statement's writes
//! (Halloween protection), while constraint checks see them, so a statement
//! cannot insert one key twice. An update or delete changes each target row
//! once, for the first input row that identifies it, and evaluates new
//! values only for those rows. `RETURNING` is evaluated over the written
//! rows (new values for insert and update, old values for delete).

use std::collections::HashSet;

use arrow_array::{BooleanArray, RecordBatch};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use interlaken_ir::interp::{Datum, RowKey};
use interlaken_ir::{Expr, Type};
use interlaken_storage::catalog::TableDef as StoredTable;
use interlaken_storage::encoding::{ColumnType, Datum as Stored};
use interlaken_storage::rows;

use super::pull::{Pull, batch, into_stream};
use super::scan::to_stored;
use crate::columnar::{build_array, datum_at};
use crate::error::ExecError;
use crate::exec::{BatchStream, ExecContext, compile_over, execute};
use crate::kernels::CompiledExpr;
use crate::memory::{Reservation, entry_bytes};
use crate::physical::{Dml, Physical};

/// What a DML operator does to each target row.
enum Write {
    /// Insert rows whose input columns fill these table columns.
    Insert(Vec<usize>),
    /// Update target rows (input positions of their columns) with new values.
    Update(Vec<usize>, Vec<(usize, CompiledExpr)>),
    /// Delete target rows (input positions of their columns).
    Delete(Vec<usize>),
}

/// A DML statement in progress.
struct DmlState {
    /// The write.
    write: Write,
    /// The input.
    input: BatchStream,
    /// The context.
    ctx: ExecContext,
    /// The IR target table.
    table: std::sync::Arc<interlaken_ir::catalog::TableDef>,
    /// The stored table, looked up on the first pull.
    stored: Option<StoredTable>,
    /// Primary keys of the target rows already changed by this statement.
    changed: HashSet<RowKey>,
    /// The memory held by `changed`.
    memory: Reservation,
    /// The compiled `RETURNING` expressions, over the target row.
    returning: Vec<CompiledExpr>,
    /// The column types of the target table.
    types: Vec<Type>,
    /// The output schema.
    schema: SchemaRef,
}

/// Convert a row to its stored form.
fn stored_row(row: &[Datum]) -> Result<Vec<Stored>, ExecError> {
    row.iter()
        .map(|d| to_stored(d).ok_or_else(|| ExecError::Unsupported(format!("storing {d:?}"))))
        .collect()
}

/// Convert a whole table row to its stored form, for columns of `types`.
///
/// Arrow carries `jsonb` as text, so text bound for a `jsonb` column (or
/// array elements) is stored as `jsonb`.
fn stored_table_row(row: &[Datum], types: &[ColumnType]) -> Result<Vec<Stored>, ExecError> {
    Ok(stored_row(row)?
        .into_iter()
        .zip(types)
        .map(|(value, ty)| as_column_type(value, *ty))
        .collect())
}

/// Return `value` as a value of column type `ty` where Arrow lost the distinction.
fn as_column_type(value: Stored, ty: ColumnType) -> Stored {
    match (value, ty) {
        (Stored::Text(s), ColumnType::Jsonb) => Stored::Jsonb(s),
        (Stored::Array(items), ColumnType::Array(element)) => Stored::Array(
            items
                .into_iter()
                .map(|item| as_column_type(item, element.column_type()))
                .collect(),
        ),
        (value, _) => value,
    }
}

impl DmlState {
    /// Return the target rows of a batch (whole table rows), one per input row.
    fn target_rows(b: &RecordBatch, key: &[usize]) -> Result<Vec<Vec<Datum>>, ExecError> {
        (0..b.num_rows())
            .map(|r| {
                key.iter()
                    .map(|&p| datum_at(b.column(p).as_ref(), r))
                    .collect()
            })
            .collect()
    }

    /// Return the input rows whose target rows this statement has not changed yet, marking them.
    fn first_occurrences(&mut self, rows: &[Vec<Datum>]) -> Result<Vec<usize>, ExecError> {
        let pk: Vec<usize> = self
            .table
            .primary_key
            .iter()
            .map(|c| usize::try_from(c.get()).expect("column id"))
            .collect();
        let mut first = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let key = RowKey(if pk.is_empty() {
                row.clone()
            } else {
                pk.iter().map(|&c| row[c].clone()).collect()
            });
            if !self.changed.contains(&key) {
                self.memory.grow(entry_bytes(&key, 0))?;
                self.changed.insert(key);
                first.push(i);
            }
        }
        Ok(first)
    }

    /// Apply the write to one input batch and return the rows `RETURNING` sees.
    async fn apply(&mut self, b: &RecordBatch) -> Result<Vec<Vec<Datum>>, ExecError> {
        let stored = self.stored.clone().expect("looked up");
        let stored = &stored;
        let txn = &self.ctx.txn.clone();
        let width = self.table.columns.len();
        let pk_of = |row: &[Datum]| -> Result<Vec<Stored>, ExecError> {
            stored_row(
                &stored
                    .primary_key()
                    .iter()
                    .map(|&c| row[c].clone())
                    .collect::<Vec<_>>(),
            )
        };
        let mut written = Vec::new();
        match &mut self.write {
            Write::Insert(columns) => {
                for r in 0..b.num_rows() {
                    let mut row = vec![Datum::Null; width];
                    for (k, &c) in columns.iter().enumerate() {
                        row[c] = datum_at(b.column(k).as_ref(), r)?;
                    }
                    rows::insert(txn, stored, stored_table_row(&row, &stored.types())?).await?;
                    written.push(row);
                }
            }
            Write::Delete(key) => {
                let key = key.clone();
                let targets = Self::target_rows(b, &key)?;
                for i in self.first_occurrences(&targets)? {
                    let row = &targets[i];
                    if rows::delete(txn, stored, &pk_of(row)?).await? {
                        written.push(row.clone());
                    }
                }
            }
            Write::Update(key, _) => {
                let key = key.clone();
                let targets = Self::target_rows(b, &key)?;
                let selected = self.first_occurrences(&targets)?;
                // New values are computed only for the rows actually updated.
                let mask: BooleanArray = (0..b.num_rows())
                    .map(|i| Some(selected.binary_search(&i).is_ok()))
                    .collect();
                let chosen = arrow_select::filter::filter_record_batch(b, &mask)?;
                let Write::Update(_, assignments) = &mut self.write else {
                    unreachable!("matched above")
                };
                let values = assignments
                    .iter_mut()
                    .map(|(c, e)| Ok((*c, e.eval(chosen.columns(), chosen.num_rows())?)))
                    .collect::<Result<Vec<_>, ExecError>>()?;
                for (k, &i) in selected.iter().enumerate() {
                    let old = &targets[i];
                    let mut new = old.clone();
                    for (c, array) in &values {
                        new[*c] = datum_at(array.as_ref(), k)?;
                    }
                    if rows::update(
                        txn,
                        stored,
                        &pk_of(old)?,
                        stored_table_row(&new, &stored.types())?,
                    )
                    .await?
                    {
                        written.push(new);
                    }
                }
            }
        }
        Ok(written)
    }

    /// Evaluate `RETURNING` over written rows.
    fn returning(&mut self, written: &[Vec<Datum>]) -> Result<RecordBatch, ExecError> {
        if self.returning.is_empty() {
            return batch(&self.schema, Vec::new(), written.len());
        }
        let columns = self
            .types
            .iter()
            .enumerate()
            .map(|(c, ty)| {
                build_array(
                    &ty.data,
                    &written.iter().map(|r| r[c].clone()).collect::<Vec<_>>(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let out = self
            .returning
            .iter_mut()
            .map(|e| e.eval(&columns, written.len()))
            .collect::<Result<Vec<_>, _>>()?;
        batch(&self.schema, out, written.len())
    }
}

impl Pull for DmlState {
    /// Write one input batch per pull and return its `RETURNING` rows.
    async fn next(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if self.stored.is_none() {
            let name = self.table.name.last().to_string();
            self.stored = Some(
                interlaken_storage::catalog::table(&self.ctx.txn, &name)
                    .await?
                    .ok_or_else(|| ExecError::Catalog(format!("table {name} is not stored")))?,
            );
        }
        let Some(b) = self.input.next().await else {
            return Ok(None);
        };
        let written = self.apply(&b?).await?;
        self.returning(&written).map(Some)
    }
}

/// The DML operation of a physical node.
pub(crate) enum DmlKind<'a> {
    /// An insert filling these table columns.
    Insert(&'a [usize]),
    /// An update of the target row at these input positions.
    Update(&'a [usize], &'a [(usize, Expr)]),
    /// A delete of the target row at these input positions.
    Delete(&'a [usize]),
}

/// Start a DML statement producing `node`'s `RETURNING` rows.
pub(crate) fn dml(
    node: &Physical,
    dml: &Dml,
    kind: DmlKind<'_>,
    ctx: &ExecContext,
) -> Result<BatchStream, ExecError> {
    let types: Vec<Type> = dml.table.columns.iter().map(|c| c.ty.clone()).collect();
    let returning = dml
        .returning
        .iter()
        .map(|e| compile_over(e, &dml.target, &types, ctx))
        .collect::<Result<_, _>>()?;
    let write = match kind {
        DmlKind::Insert(columns) => Write::Insert(columns.to_vec()),
        DmlKind::Update(key, assignments) => Write::Update(
            key.to_vec(),
            assignments
                .iter()
                .map(|(c, e)| {
                    Ok((
                        *c,
                        compile_over(e, &dml.input.layout, &dml.input.types, ctx)?,
                    ))
                })
                .collect::<Result<_, ExecError>>()?,
        ),
        DmlKind::Delete(key) => Write::Delete(key.to_vec()),
    };
    Ok(into_stream(DmlState {
        write,
        input: execute(&dml.input, ctx),
        ctx: ctx.clone(),
        table: dml.table.clone(),
        stored: None,
        changed: HashSet::new(),
        memory: ctx.reserve(node, "ModifyTable"),
        returning,
        types,
        schema: node.schema.clone(),
    }))
}
