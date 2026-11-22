//! The system tables that hold the catalog (ADR-0007).
//!
//! Their definitions are built in, so the catalog is read with the same row
//! layer as user tables without a bootstrap step:
//!
//! | Table | ID | Columns | Primary key | Unique index |
//! | --- | --- | --- | --- | --- |
//! | `sys_tables` | 1 | table_id, name, schema_version, primary_key | table_id | `sys_tables_name` (name) |
//! | `sys_columns` | 2 | table_id, column_id, name, type, nullable | table_id, column_id | |
//! | `sys_indexes` | 3 | table_id, index_id, name, columns, unique, nulls_distinct | table_id, index_id | `sys_indexes_name` (name) |
//! | `sys_counters` | 4 | name, value | name | |
//!
//! Column lists are stored as `bytea`: a primary key as `u16` LE column
//! positions, an index as (`u16` LE position, flags) pairs where flag bit 0
//! is DESC and bit 1 is NULLS FIRST.

use std::sync::OnceLock;

use super::defs::{ColumnDef, IndexColumn, IndexDef, IndexId, TableDef, TableId};
use crate::encoding::{ColumnType, Datum, Direction, NullOrder, SortOrder};
use crate::error::CatalogError;

/// `sys_tables` column positions.
pub(crate) mod tables {
    pub(crate) const TABLE_ID: usize = 0;
    pub(crate) const NAME: usize = 1;
    pub(crate) const SCHEMA_VERSION: usize = 2;
    pub(crate) const PRIMARY_KEY: usize = 3;
}

/// `sys_columns` column positions.
pub(crate) mod columns {
    pub(crate) const COLUMN_ID: usize = 1;
    pub(crate) const NAME: usize = 2;
    pub(crate) const TYPE: usize = 3;
    pub(crate) const NULLABLE: usize = 4;
}

/// `sys_indexes` column positions.
pub(crate) mod indexes {
    pub(crate) const TABLE_ID: usize = 0;
    pub(crate) const INDEX_ID: usize = 1;
    pub(crate) const NAME: usize = 2;
    pub(crate) const COLUMNS: usize = 3;
    pub(crate) const UNIQUE: usize = 4;
    pub(crate) const NULLS_DISTINCT: usize = 5;
}

/// The counter row in `sys_counters` that hands out table and index IDs.
pub(crate) const OBJECT_ID_COUNTER: &str = "object_id";

/// The four system table definitions.
pub(crate) struct SystemTables {
    pub(crate) tables: TableDef,
    pub(crate) columns: TableDef,
    pub(crate) indexes: TableDef,
    pub(crate) counters: TableDef,
}

/// Return the built-in system table definitions.
pub(crate) fn system() -> &'static SystemTables {
    static SYSTEM: OnceLock<SystemTables> = OnceLock::new();
    SYSTEM.get_or_init(|| SystemTables {
        tables: system_table(
            1,
            "sys_tables",
            &[
                ("table_id", ColumnType::Int4),
                ("name", ColumnType::Text),
                ("schema_version", ColumnType::Int4),
                ("primary_key", ColumnType::Bytea),
            ],
            &[tables::TABLE_ID],
            Some(("sys_tables_name", tables::NAME)),
        ),
        columns: system_table(
            2,
            "sys_columns",
            &[
                ("table_id", ColumnType::Int4),
                ("column_id", ColumnType::Int4),
                ("name", ColumnType::Text),
                ("type", ColumnType::Int2),
                ("nullable", ColumnType::Bool),
            ],
            &[0, columns::COLUMN_ID],
            None,
        ),
        indexes: system_table(
            3,
            "sys_indexes",
            &[
                ("table_id", ColumnType::Int4),
                ("index_id", ColumnType::Int4),
                ("name", ColumnType::Text),
                ("columns", ColumnType::Bytea),
                ("unique", ColumnType::Bool),
                ("nulls_distinct", ColumnType::Bool),
            ],
            &[indexes::TABLE_ID, indexes::INDEX_ID],
            Some(("sys_indexes_name", indexes::NAME)),
        ),
        counters: system_table(
            4,
            "sys_counters",
            &[("name", ColumnType::Text), ("value", ColumnType::Int8)],
            &[0],
            None,
        ),
    })
}

/// Construct a system table definition with all columns NOT NULL.
fn system_table(
    id: i32,
    name: &str,
    columns: &[(&str, ColumnType)],
    primary_key: &[usize],
    unique: Option<(&str, usize)>,
) -> TableDef {
    TableDef {
        id: TableId(id),
        name: name.to_owned(),
        schema_version: 1,
        columns: columns
            .iter()
            .map(|(name, ty)| ColumnDef::new(*name, *ty).not_null())
            .collect(),
        primary_key: primary_key.to_vec(),
        indexes: unique
            .map(|(name, column)| IndexDef {
                id: IndexId(1),
                name: name.to_owned(),
                columns: vec![IndexColumn {
                    column,
                    order: SortOrder::ASC,
                }],
                unique: true,
                nulls_distinct: true,
            })
            .into_iter()
            .collect(),
    }
}

/// Encode a primary key column list.
pub(crate) fn encode_primary_key(columns: &[usize]) -> Vec<u8> {
    columns
        .iter()
        .flat_map(|&c| {
            u16::try_from(c)
                .expect("column count fits u16")
                .to_le_bytes()
        })
        .collect()
}

/// Decode a primary key column list.
pub(crate) fn decode_primary_key(bytes: &[u8]) -> Result<Vec<usize>, CatalogError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(CatalogError::Corrupt("primary key column list"));
    }
    Ok(bytes
        .chunks_exact(2)
        .map(|c| usize::from(u16::from_le_bytes([c[0], c[1]])))
        .collect())
}

/// Encode an index column list.
pub(crate) fn encode_index_columns(columns: &[IndexColumn]) -> Vec<u8> {
    let mut out = Vec::with_capacity(columns.len() * 3);
    for c in columns {
        out.extend_from_slice(&u16::try_from(c.column).expect("fits u16").to_le_bytes());
        let mut flags = 0u8;
        if c.order.direction == Direction::Desc {
            flags |= 1;
        }
        if c.order.nulls == NullOrder::First {
            flags |= 2;
        }
        out.push(flags);
    }
    out
}

/// Decode an index column list.
pub(crate) fn decode_index_columns(bytes: &[u8]) -> Result<Vec<IndexColumn>, CatalogError> {
    if !bytes.len().is_multiple_of(3) {
        return Err(CatalogError::Corrupt("index column list"));
    }
    bytes
        .chunks_exact(3)
        .map(|c| {
            if c[2] & !3 != 0 {
                return Err(CatalogError::Corrupt("index column flags"));
            }
            let direction = if c[2] & 1 != 0 {
                Direction::Desc
            } else {
                Direction::Asc
            };
            let nulls = if c[2] & 2 != 0 {
                NullOrder::First
            } else {
                NullOrder::Last
            };
            Ok(IndexColumn {
                column: usize::from(u16::from_le_bytes([c[0], c[1]])),
                order: SortOrder::new(direction, nulls),
            })
        })
        .collect()
}

/// Return the value of an `int4` catalog column.
pub(crate) fn int4(row: &[Datum], column: usize) -> Result<i32, CatalogError> {
    match row.get(column) {
        Some(Datum::Int4(v)) => Ok(*v),
        _ => Err(CatalogError::Corrupt("expected int4")),
    }
}

/// Return the value of a `text` catalog column.
pub(crate) fn text(row: &[Datum], column: usize) -> Result<&str, CatalogError> {
    match row.get(column) {
        Some(Datum::Text(v)) => Ok(v),
        _ => Err(CatalogError::Corrupt("expected text")),
    }
}

/// Return the value of a `bytea` catalog column.
pub(crate) fn bytea(row: &[Datum], column: usize) -> Result<&[u8], CatalogError> {
    match row.get(column) {
        Some(Datum::Bytea(v)) => Ok(v),
        _ => Err(CatalogError::Corrupt("expected bytea")),
    }
}

/// Return the value of a `bool` catalog column.
pub(crate) fn boolean(row: &[Datum], column: usize) -> Result<bool, CatalogError> {
    match row.get(column) {
        Some(Datum::Bool(v)) => Ok(*v),
        _ => Err(CatalogError::Corrupt("expected bool")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert column lists round-trip and corrupt lists are rejected.
    fn column_lists_round_trip() {
        let pk = vec![3, 0, 7];
        assert_eq!(decode_primary_key(&encode_primary_key(&pk)).unwrap(), pk);
        let index = vec![
            IndexColumn {
                column: 2,
                order: SortOrder::DESC,
            },
            IndexColumn {
                column: 0,
                order: SortOrder::ASC,
            },
        ];
        assert_eq!(
            decode_index_columns(&encode_index_columns(&index)).unwrap(),
            index
        );
        assert!(decode_primary_key(&[1]).is_err());
        assert!(decode_index_columns(&[0, 0, 4]).is_err());
    }
}
