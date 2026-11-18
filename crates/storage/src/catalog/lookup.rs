//! Reading table definitions from the system tables.
//!
//! Lookups see the transaction's snapshot plus all its buffered writes,
//! including the current statement's, so DDL can build on earlier DDL in
//! the same statement.

use super::defs::{ColumnDef, IndexDef, IndexId, TableDef, TableId};
use super::system::{
    boolean, bytea, columns, decode_index_columns, decode_primary_key, indexes, int4,
    system as sys, tables, text,
};
use crate::encoding::{ColumnType, Datum};
use crate::rows::{Row, pk_from_index_entry, pk_key, read_latest, read_range_latest, unique_key};
use crate::scan::KeyRange;
use crate::{CatalogError, Result, Txn};

/// Return the definition of the table called `name`, or `None`.
///
/// # Errors
///
/// Returns [`Error::Catalog`](crate::Error::Catalog) if the catalog is
/// corrupt, or a storage error.
pub async fn table(txn: &Txn, name: &str) -> Result<Option<TableDef>> {
    let sys = sys();
    let Some(pk) = lookup_unique(txn, &sys.tables, Datum::Text(name.to_owned())).await? else {
        return Ok(None);
    };
    let key = pk_key(&sys.tables, &pk)?;
    match read_latest(txn, &sys.tables, &key).await? {
        Some(row) => Ok(Some(load(txn, &row).await?)),
        None => Err(CatalogError::Corrupt("name index points to a missing table").into()),
    }
}

/// Return the definition of the table with ID `id`, or `None`.
///
/// # Errors
///
/// As for [`table`].
pub async fn table_by_id(txn: &Txn, id: TableId) -> Result<Option<TableDef>> {
    let sys = sys();
    let key = pk_key(&sys.tables, &[Datum::Int4(id.get())])?;
    match read_latest(txn, &sys.tables, &key).await? {
        Some(row) => Ok(Some(load(txn, &row).await?)),
        None => Ok(None),
    }
}

/// Return the definitions of every user table, in ID order.
///
/// # Errors
///
/// As for [`table`].
pub async fn tables(txn: &Txn) -> Result<Vec<TableDef>> {
    let sys = sys();
    let mut defs = Vec::new();
    for row in read_rows(txn, &sys.tables, &[]).await? {
        defs.push(load(txn, &row).await?);
    }
    Ok(defs)
}

/// Return the primary key of the row whose unique-indexed name is `name`, in a system table.
pub(crate) async fn lookup_unique(
    txn: &Txn,
    table: &TableDef,
    name: Datum,
) -> Result<Option<Vec<Datum>>> {
    let index = &table.indexes()[0];
    let key = unique_key(table, index, &[name])?;
    let Some(value) = txn
        .get_many_latest(vec![key.clone()])
        .await?
        .pop()
        .flatten()
    else {
        return Ok(None);
    };
    Ok(Some(pk_from_index_entry(table, index, &key, &value)?))
}

/// Read every row of `table` whose primary key starts with `pk_prefix`.
pub(crate) async fn read_rows(
    txn: &Txn,
    table: &TableDef,
    pk_prefix: &[Datum],
) -> Result<Vec<Row>> {
    let range = KeyRange::prefix(&pk_key(table, pk_prefix)?);
    let types = table.types();
    read_range_latest(txn, range)
        .await?
        .into_iter()
        .map(|(_, value)| Ok(crate::encoding::decode_row(&value, &types)?.1))
        .collect()
}

/// Build a table definition from its `sys_tables` row and its column and index rows.
pub(crate) async fn load(txn: &Txn, row: &[Datum]) -> Result<TableDef> {
    let sys = sys();
    let id = int4(row, tables::TABLE_ID)?;
    let schema_version = u32::try_from(int4(row, tables::SCHEMA_VERSION)?)
        .map_err(|_| CatalogError::Corrupt("negative schema version"))?;
    let key = [Datum::Int4(id)];

    let mut column_defs = Vec::new();
    for (position, column) in read_rows(txn, &sys.columns, &key).await?.iter().enumerate() {
        if usize::try_from(int4(column, columns::COLUMN_ID)?) != Ok(position) {
            return Err(CatalogError::Corrupt("column ids are not contiguous").into());
        }
        let code = match column.get(columns::TYPE) {
            Some(Datum::Int2(code)) => *code,
            _ => return Err(CatalogError::Corrupt("expected int2").into()),
        };
        column_defs.push(ColumnDef {
            name: text(column, columns::NAME)?.to_owned(),
            ty: ColumnType::from_code(code).ok_or(CatalogError::Corrupt("unknown column type"))?,
            nullable: boolean(column, columns::NULLABLE)?,
        });
    }

    let mut index_defs = Vec::new();
    for index in read_rows(txn, &sys.indexes, &key).await? {
        let columns = decode_index_columns(bytea(&index, indexes::COLUMNS)?)?;
        if columns.iter().any(|c| c.column >= column_defs.len()) {
            return Err(CatalogError::Corrupt("index column out of range").into());
        }
        index_defs.push(IndexDef {
            id: IndexId(int4(&index, indexes::INDEX_ID)?),
            name: text(&index, indexes::NAME)?.to_owned(),
            columns,
            unique: boolean(&index, indexes::UNIQUE)?,
        });
    }

    let primary_key = decode_primary_key(bytea(row, tables::PRIMARY_KEY)?)?;
    if primary_key.is_empty() || primary_key.iter().any(|&c| c >= column_defs.len()) {
        return Err(CatalogError::Corrupt("primary key column out of range").into());
    }
    Ok(TableDef {
        id: TableId(id),
        name: text(row, tables::NAME)?.to_owned(),
        schema_version,
        columns: column_defs,
        primary_key,
        indexes: index_defs,
    })
}
