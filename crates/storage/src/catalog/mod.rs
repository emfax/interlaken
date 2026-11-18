//! The catalog: table and index definitions in system tables, and transactional DDL (ADR-0007).

mod ddl;
mod defs;
mod lookup;
mod system;

pub use ddl::{add_column, create_index, create_table, drop_index, drop_table};
pub use defs::{
    ColumnDef, IndexColumn, IndexDef, IndexId, IndexSpec, TableDef, TableId, TableSpec,
};
pub use lookup::{table, table_by_id, tables};
pub(crate) use system::system;
