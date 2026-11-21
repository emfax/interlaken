//! Foreign key checks for the reference interpreter (ADR-0035).
//!
//! After each DML operator changes a table, every foreign key from or to
//! that table must hold over the whole database, which is the reference
//! meaning of the engine's per-statement checks.

use std::collections::HashSet;

use super::datum::{Datum, RowKey};
use super::error::EvalError;
use super::run::Database;
use crate::catalog::{Catalog, ForeignKeyDef};
use crate::names::{ColumnId, TableId};

/// Return the position of column `id`.
fn at(id: ColumnId) -> usize {
    usize::try_from(id.get()).expect("column id")
}

/// Check every foreign key whose referencing or referenced table is `table`.
///
/// # Errors
///
/// Returns [`EvalError::ForeignKey`] for the first violated foreign key, in name order.
pub(crate) fn check(catalog: &dyn Catalog, db: &Database, table: TableId) -> Result<(), EvalError> {
    for fk in catalog.foreign_keys() {
        if fk.table == table || fk.referenced == table {
            check_one(catalog, db, &fk)?;
        }
    }
    Ok(())
}

/// Check that every row of `fk`'s referencing table refers to an existing row.
fn check_one(catalog: &dyn Catalog, db: &Database, fk: &ForeignKeyDef) -> Result<(), EvalError> {
    let name = |id: TableId| {
        catalog
            .table_by_id(id)
            .map_or_else(|| id.get().to_string(), |t| t.name.to_string())
    };
    let keys: HashSet<RowKey> = db
        .rows(fk.referenced)
        .iter()
        .map(|row| {
            RowKey(
                fk.referenced_columns
                    .iter()
                    .map(|&c| row[at(c)].clone())
                    .collect(),
            )
        })
        .collect();
    for row in db.rows(fk.table) {
        let values: Vec<Datum> = fk.columns.iter().map(|&c| row[at(c)].clone()).collect();
        let nulls = values.iter().filter(|v| v.is_null()).count();
        if nulls == values.len() || (nulls > 0 && !fk.match_full) {
            continue;
        }
        if nulls > 0 {
            return Err(EvalError::ForeignKey(format!(
                "a row of {} has some but not all of the columns of foreign key {}, which \
                 matches FULL, NULL",
                name(fk.table),
                fk.name
            )));
        }
        if !keys.contains(&RowKey(values)) {
            return Err(EvalError::ForeignKey(format!(
                "foreign key {}: a row of {} refers to a row of {} that doesn't exist",
                fk.name,
                name(fk.table),
                name(fk.referenced)
            )));
        }
    }
    Ok(())
}
