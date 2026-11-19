//! Transactional DDL (ADR-0007).
//!
//! DDL writes the system tables through the row layer, so it commits or
//! aborts with the rest of the transaction. Every change to a table rewrites
//! its `sys_tables` row (bumping the schema version), which conflicts with
//! concurrent DML on that table at every isolation level. DDL that reads a
//! table's data (index backfill, dropping) registers that range as a
//! dependency, so concurrent DML that committed first aborts the DDL instead
//! of being missed.

use std::collections::HashSet;

use super::defs::{ColumnDef, IndexColumn, IndexId, IndexSpec, TableDef, TableId, TableSpec};
use super::lookup::{lookup_unique, table, table_by_id};
use super::system::{OBJECT_ID_COUNTER, encode_index_columns, encode_primary_key, int4, system};
use crate::encoding::{Datum, decode_row};
use crate::rows::{self, index_range, pk_key, read_latest, read_range_latest, table_range};
use crate::{CatalogError, ConstraintViolation, Error, Result, Txn};

/// Most columns a table may have, as in Postgres.
const MAX_COLUMNS: usize = 1600;

/// Create a table and return its definition.
///
/// Primary key columns become NOT NULL.
///
/// # Errors
///
/// - [`CatalogError::TableExists`] if the name is taken.
/// - [`CatalogError::InvalidDefinition`] if the name is empty or reserved
///   (`sys_` prefix), there are no columns or too many, a column name
///   repeats, or the primary key is empty, repeats a column, names an
///   unknown column or includes an array column.
/// - A storage error, or [`Error::ReadOnlyTransaction`].
pub async fn create_table(txn: &Txn, spec: TableSpec) -> Result<TableDef> {
    check_name("table", &spec.name)?;
    if spec.columns.is_empty() || spec.columns.len() > MAX_COLUMNS {
        return Err(invalid(format!(
            "a table needs between 1 and {MAX_COLUMNS} columns"
        )));
    }
    let mut seen = HashSet::new();
    for column in &spec.columns {
        check_name("column", &column.name)?;
        if !seen.insert(column.name.as_str()) {
            return Err(invalid(format!("column \"{}\" is repeated", column.name)));
        }
    }
    if spec.primary_key.is_empty() {
        return Err(invalid("a table needs a primary key".to_owned()));
    }
    let mut primary_key = Vec::new();
    for name in &spec.primary_key {
        let position = spec
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| CatalogError::NoSuchColumn {
                table: spec.name.clone(),
                column: name.clone(),
            })?;
        if primary_key.contains(&position) {
            return Err(invalid(format!("primary key repeats column \"{name}\"")));
        }
        if !spec.columns[position].ty.is_keyable() {
            return Err(invalid(format!(
                "primary key column \"{name}\" has type {}, which can't be a key",
                spec.columns[position].ty
            )));
        }
        primary_key.push(position);
    }
    if table(txn, &spec.name).await?.is_some() {
        return Err(CatalogError::TableExists(spec.name).into());
    }

    let id = next_id(txn).await?;
    let sys = system();
    rows::insert(
        txn,
        &sys.tables,
        vec![
            Datum::Int4(id),
            Datum::Text(spec.name.clone()),
            Datum::Int4(1),
            Datum::Bytea(encode_primary_key(&primary_key)),
        ],
    )
    .await
    .map_err(|e| exists_as(e, CatalogError::TableExists(spec.name.clone())))?;

    let mut columns = spec.columns;
    for &position in &primary_key {
        columns[position].nullable = false;
    }
    for (position, column) in columns.iter().enumerate() {
        insert_column(txn, id, position, column).await?;
    }
    Ok(TableDef {
        id: TableId(id),
        name: spec.name,
        schema_version: 1,
        columns,
        primary_key,
        indexes: Vec::new(),
    })
}

/// Drop the table called `name`, with all its rows and indexes.
///
/// # Errors
///
/// [`CatalogError::NoSuchTable`] if there is no such table, or a storage error.
pub async fn drop_table(txn: &Txn, name: &str) -> Result<()> {
    let def = table(txn, name)
        .await?
        .ok_or_else(|| CatalogError::NoSuchTable(name.to_owned()))?;
    delete_range(txn, table_range(def.id())).await?;
    let sys = system();
    let id = Datum::Int4(def.id().get());
    for position in 0..def.columns().len() {
        rows::delete(txn, &sys.columns, &[id.clone(), column_id(position)]).await?;
    }
    for index in def.indexes() {
        rows::delete(
            txn,
            &sys.indexes,
            &[id.clone(), Datum::Int4(index.id().get())],
        )
        .await?;
    }
    rows::delete(txn, &sys.tables, &[id]).await?;
    Ok(())
}

/// Create an index, fill it from the table's existing rows, and return the
/// updated table definition.
///
/// # Errors
///
/// - [`CatalogError::NoSuchTable`] or [`CatalogError::NoSuchColumn`] if the
///   table or a column doesn't exist.
/// - [`CatalogError::IndexExists`] if the name is taken.
/// - [`CatalogError::InvalidDefinition`] if the name is empty or reserved,
///   or the column list is empty, repeats a column or includes an array column.
/// - [`ConstraintViolation::Unique`] if a unique index would have duplicates.
/// - A storage error, or [`Error::ReadOnlyTransaction`].
pub async fn create_index(txn: &Txn, spec: IndexSpec) -> Result<TableDef> {
    check_name("index", &spec.name)?;
    let def = table(txn, &spec.table)
        .await?
        .ok_or_else(|| CatalogError::NoSuchTable(spec.table.clone()))?;
    if spec.columns.is_empty() {
        return Err(invalid("an index needs at least one column".to_owned()));
    }
    let mut columns = Vec::new();
    for (name, order) in &spec.columns {
        let column = def
            .column_position(name)
            .ok_or_else(|| CatalogError::NoSuchColumn {
                table: def.name().to_owned(),
                column: name.clone(),
            })?;
        if columns.iter().any(|c: &IndexColumn| c.column == column) {
            return Err(invalid(format!("index repeats column \"{name}\"")));
        }
        let ty = def.columns()[column].ty;
        if !ty.is_keyable() {
            return Err(invalid(format!(
                "index column \"{name}\" has type {ty}, which can't be indexed"
            )));
        }
        columns.push(IndexColumn {
            column,
            order: *order,
        });
    }
    let sys = system();
    if lookup_unique(txn, &sys.indexes, Datum::Text(spec.name.clone()))
        .await?
        .is_some()
    {
        return Err(CatalogError::IndexExists(spec.name).into());
    }

    let id = next_id(txn).await?;
    rows::insert(
        txn,
        &sys.indexes,
        vec![
            Datum::Int4(def.id().get()),
            Datum::Int4(id),
            Datum::Text(spec.name.clone()),
            Datum::Bytea(encode_index_columns(&columns)),
            Datum::Bool(spec.unique),
        ],
    )
    .await
    .map_err(|e| exists_as(e, CatalogError::IndexExists(spec.name.clone())))?;
    bump_schema_version(txn, &def).await?;

    let def = table_by_id(txn, def.id())
        .await?
        .ok_or(CatalogError::Corrupt("table vanished during CREATE INDEX"))?;
    let index = def
        .indexes()
        .iter()
        .find(|i| i.id() == IndexId(id))
        .ok_or(CatalogError::Corrupt("new index missing"))?;
    backfill(txn, &def, index).await?;
    Ok(def)
}

/// Drop the index called `name` and return the updated table definition.
///
/// # Errors
///
/// [`CatalogError::NoSuchIndex`] if there is no such index, or a storage error.
pub async fn drop_index(txn: &Txn, name: &str) -> Result<TableDef> {
    let sys = system();
    let pk = lookup_unique(txn, &sys.indexes, Datum::Text(name.to_owned()))
        .await?
        .ok_or_else(|| CatalogError::NoSuchIndex(name.to_owned()))?;
    let table_id = TableId(int4(&pk, 0)?);
    let index_id = IndexId(int4(&pk, 1)?);
    let def = table_by_id(txn, table_id)
        .await?
        .ok_or(CatalogError::Corrupt("index of a missing table"))?;
    rows::delete(txn, &sys.indexes, &pk).await?;
    delete_range(txn, index_range(table_id, index_id)).await?;
    bump_schema_version(txn, &def).await?;
    Ok(table_by_id(txn, table_id)
        .await?
        .ok_or(CatalogError::Corrupt("table vanished during DROP INDEX"))?)
}

/// Add a nullable column at the end of the table called `table_name` and
/// return the updated definition. Existing rows read the new column as NULL.
///
/// # Errors
///
/// - [`CatalogError::NoSuchTable`] if there is no such table.
/// - [`CatalogError::ColumnExists`] if the name is taken.
/// - [`CatalogError::InvalidDefinition`] if the column is NOT NULL (no
///   defaults exist to fill existing rows), the name is empty or reserved,
///   or the table already has the maximum number of columns.
/// - A storage error, or [`Error::ReadOnlyTransaction`].
pub async fn add_column(txn: &Txn, table_name: &str, column: ColumnDef) -> Result<TableDef> {
    check_name("column", &column.name)?;
    if !column.nullable {
        return Err(invalid(
            "an added column must be nullable: there are no defaults to fill existing rows"
                .to_owned(),
        ));
    }
    let def = table(txn, table_name)
        .await?
        .ok_or_else(|| CatalogError::NoSuchTable(table_name.to_owned()))?;
    if def.column_position(&column.name).is_some() {
        return Err(CatalogError::ColumnExists {
            table: def.name().to_owned(),
            column: column.name,
        }
        .into());
    }
    if def.columns().len() >= MAX_COLUMNS {
        return Err(invalid(format!(
            "a table can have at most {MAX_COLUMNS} columns"
        )));
    }
    insert_column(txn, def.id().get(), def.columns().len(), &column).await?;
    bump_schema_version(txn, &def).await?;
    Ok(table_by_id(txn, def.id())
        .await?
        .ok_or(CatalogError::Corrupt("table vanished during ADD COLUMN"))?)
}

/// Write the index entries of every existing row of `def` into `index`.
async fn backfill(txn: &Txn, def: &TableDef, index: &super::defs::IndexDef) -> Result<()> {
    let range = index_range(def.id(), IndexId::PRIMARY);
    let rows = read_range_latest(txn, range.clone()).await?;
    txn.register_dependency_range(range);
    let types = def.types();
    let mut exclusive = HashSet::new();
    for (_, value) in rows {
        let row = decode_row(&value, &types)?.1;
        let entry = crate::rows::index_entry_of(def, index, &row)?;
        if entry.exclusive && !exclusive.insert(entry.key.clone()) {
            return Err(ConstraintViolation::Unique {
                index: index.name().to_owned(),
            }
            .into());
        }
        txn.put(entry.key, entry.value)?;
    }
    Ok(())
}

/// Delete every key in `range`, depending on the range so concurrent writes into it abort us.
async fn delete_range(txn: &Txn, range: crate::KeyRange) -> Result<()> {
    let entries = read_range_latest(txn, range.clone()).await?;
    txn.register_dependency_range(range);
    for (key, _) in entries {
        txn.delete(key)?;
    }
    Ok(())
}

/// Insert the `sys_columns` row of one column.
async fn insert_column(txn: &Txn, table: i32, position: usize, column: &ColumnDef) -> Result<()> {
    rows::insert(
        txn,
        &system().columns,
        vec![
            Datum::Int4(table),
            column_id(position),
            Datum::Text(column.name.clone()),
            Datum::Int2(column.ty.code()),
            Datum::Bool(column.nullable),
        ],
    )
    .await
}

/// Rewrite `def`'s `sys_tables` row with the next schema version.
async fn bump_schema_version(txn: &Txn, def: &TableDef) -> Result<()> {
    let version = i32::try_from(def.schema_version() + 1)
        .map_err(|_| invalid("schema version space exhausted".to_owned()))?;
    let id = Datum::Int4(def.id().get());
    let row = vec![
        id.clone(),
        Datum::Text(def.name().to_owned()),
        Datum::Int4(version),
        Datum::Bytea(encode_primary_key(def.primary_key())),
    ];
    if !rows::update(txn, &system().tables, &[id], row).await? {
        return Err(CatalogError::Corrupt("table row missing").into());
    }
    Ok(())
}

/// Allocate the next table or index ID from the counter row.
///
/// Every allocation writes the same key, so concurrent DDL conflicts and
/// one side retries.
async fn next_id(txn: &Txn) -> Result<i32> {
    let counters = &system().counters;
    let name = Datum::Text(OBJECT_ID_COUNTER.to_owned());
    let key = pk_key(counters, std::slice::from_ref(&name))?;
    let current = match read_latest(txn, counters, &key).await? {
        Some(row) => match row.get(1) {
            Some(Datum::Int8(v)) => Some(*v),
            _ => return Err(CatalogError::Corrupt("expected int8").into()),
        },
        None => None,
    };
    let next = current.map_or(i64::from(TableId::FIRST_USER), |v| v + 1);
    let id = i32::try_from(next).map_err(|_| invalid("object ID space exhausted".to_owned()))?;
    let row = vec![name.clone(), Datum::Int8(next)];
    if current.is_some() {
        rows::update(txn, counters, &[name], row).await?;
    } else {
        rows::insert(txn, counters, row).await?;
    }
    Ok(id)
}

/// Return the `sys_columns` column ID of the column at `position`.
fn column_id(position: usize) -> Datum {
    Datum::Int4(i32::try_from(position).expect("column count is bounded"))
}

/// Reject empty names and the reserved `sys_` prefix.
fn check_name(kind: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(invalid(format!("{kind} name is empty")));
    }
    if kind != "column" && name.starts_with("sys_") {
        return Err(invalid(format!(
            "{kind} names starting with \"sys_\" are reserved"
        )));
    }
    Ok(())
}

/// Construct an invalid-definition error.
fn invalid(message: String) -> Error {
    CatalogError::InvalidDefinition(message).into()
}

/// Replace a unique violation on a catalog name index with `exists`.
fn exists_as(error: Error, exists: CatalogError) -> Error {
    match error {
        Error::Constraint(ConstraintViolation::Unique { .. }) => exists.into(),
        other => other,
    }
}
