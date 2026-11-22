//! Key layouts of rows and index entries (ADR-0007).
//!
//! | Entry | Key | Value |
//! | --- | --- | --- |
//! | Row | `(table_id, 0, pk...)` | Row format |
//! | Non-unique index | `(table_id, index_id, columns..., pk...)` | Empty |
//! | Unique index | `(table_id, index_id, columns..., padding...)` | `(pk...)` |
//!
//! Primary key columns are ascending. A unique index entry is padded to the
//! same arity as a non-unique one: when every indexed value is non-NULL the
//! padding is one NULL per primary key column, so two rows with equal values
//! map to the same key and collide; when any indexed value is NULL the
//! padding is the row's primary key, so rows with NULLs never collide
//! (NULLs are distinct, as in Postgres).

use bytes::Bytes;

use crate::catalog::{IndexDef, IndexId, TableDef, TableId};
use crate::encoding::{Datum, KeyBuilder, SortOrder, decode_tuple};
use crate::scan::{Key, KeyRange};
use crate::{CatalogError, Result};

/// Return the key prefix shared by every entry of `table`.
pub(crate) fn table_prefix(table: TableId) -> Key {
    let mut builder = KeyBuilder::new();
    builder.push(&Datum::Int4(table.get()), SortOrder::ASC);
    Bytes::from(builder.finish().expect("short key"))
}

/// Return the key prefix shared by every entry of one index of `table` (index 0 for rows).
pub(crate) fn index_prefix(table: TableId, index: IndexId) -> Key {
    Bytes::from(prefix_builder(table, index).finish().expect("short key"))
}

/// Return the range of every entry of `table`, rows and indexes.
pub(crate) fn table_range(table: TableId) -> KeyRange {
    KeyRange::prefix(&table_prefix(table))
}

/// Return the range of one index of `table` (index 0 for rows).
pub(crate) fn index_range(table: TableId, index: IndexId) -> KeyRange {
    KeyRange::prefix(&index_prefix(table, index))
}

/// Start a key with the table and index IDs.
fn prefix_builder(table: TableId, index: IndexId) -> KeyBuilder {
    let mut builder = KeyBuilder::new();
    builder
        .push(&Datum::Int4(table.get()), SortOrder::ASC)
        .push(&Datum::Int4(index.get()), SortOrder::ASC);
    builder
}

/// Encode the row key of the row whose primary key values are `pk`.
///
/// `pk` may be a prefix of the primary key, for scans.
pub(crate) fn pk_key(table: &TableDef, pk: &[Datum]) -> Result<Key> {
    let mut builder = prefix_builder(table.id(), IndexId::PRIMARY);
    for value in pk {
        builder.push(value, SortOrder::ASC);
    }
    Ok(Bytes::from(builder.finish()?))
}

/// Return the primary key values of `row`.
pub(crate) fn pk_of(table: &TableDef, row: &[Datum]) -> Vec<Datum> {
    table
        .primary_key()
        .iter()
        .map(|&c| row[c].clone())
        .collect()
}

/// Encode the row key of `row`.
pub(crate) fn row_key(table: &TableDef, row: &[Datum]) -> Result<Key> {
    pk_key(table, &pk_of(table, row))
}

/// One index entry of a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexEntry {
    pub(crate) key: Key,
    pub(crate) value: Bytes,
    /// `true` if another row with equal values would map to the same key:
    /// a unique index entry whose indexed values are all non-NULL, or any
    /// entry of a unique index whose NULLs aren't distinct.
    pub(crate) exclusive: bool,
}

/// Encode the entry of `row` in `index`.
pub(crate) fn index_entry(table: &TableDef, index: &IndexDef, row: &[Datum]) -> Result<IndexEntry> {
    let mut builder = prefix_builder(table.id(), index.id());
    let mut any_null = false;
    for column in index.columns() {
        let value = &row[column.column];
        any_null |= value.is_null();
        builder.push(value, column.order);
    }
    // A unique key with a NULL conflicts only if NULLs aren't distinct.
    let exclusive = index.is_unique() && (!any_null || !index.nulls_distinct());
    for &c in table.primary_key() {
        let padding = if exclusive { &Datum::Null } else { &row[c] };
        builder.push(padding, SortOrder::ASC);
    }
    let key = Bytes::from(builder.finish()?);
    let value = if index.is_unique() {
        let mut pk = KeyBuilder::new();
        for &c in table.primary_key() {
            pk.push(&row[c], SortOrder::ASC);
        }
        Bytes::from(pk.finish()?)
    } else {
        Bytes::new()
    };
    Ok(IndexEntry {
        key,
        value,
        exclusive,
    })
}

/// Encode the exclusive entry key of `index` for indexed values `values`, all non-NULL.
///
/// A unique index holds at most one entry under this key.
pub(crate) fn unique_key(table: &TableDef, index: &IndexDef, values: &[Datum]) -> Result<Key> {
    let mut row = vec![Datum::Null; table.columns().len()];
    for (column, value) in index.columns().iter().zip(values) {
        row[column.column] = value.clone();
    }
    Ok(index_entry(table, index, &row)?.key)
}

/// Encode the key prefix of `index` entries whose leading indexed values are `values`.
pub(crate) fn index_values_prefix(
    table: &TableDef,
    index: &IndexDef,
    values: &[Datum],
) -> Result<Key> {
    if values.len() > index.columns().len() {
        return Err(CatalogError::InvalidDefinition(format!(
            "index \"{}\" has {} columns, {} values given",
            index.name(),
            index.columns().len(),
            values.len()
        ))
        .into());
    }
    let mut builder = prefix_builder(table.id(), index.id());
    for (value, column) in values.iter().zip(index.columns()) {
        builder.push(value, column.order);
    }
    Ok(Bytes::from(builder.finish()?))
}

/// Decode the primary key values from an index entry.
pub(crate) fn pk_from_index_entry(
    table: &TableDef,
    index: &IndexDef,
    key: &[u8],
    value: &[u8],
) -> Result<Vec<Datum>> {
    let pk_len = table.primary_key().len();
    if index.is_unique() {
        return Ok(decode_tuple(value, &vec![SortOrder::ASC; pk_len])?);
    }
    let mut orders = vec![SortOrder::ASC, SortOrder::ASC];
    orders.extend(index.columns().iter().map(|c| c.order));
    orders.extend(std::iter::repeat_n(SortOrder::ASC, pk_len));
    let mut values = decode_tuple(key, &orders)?;
    Ok(values.split_off(values.len() - pk_len))
}
