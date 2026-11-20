//! The storage-backed IR catalog and engine DDL (ADR-0013 phase 3, ADR-0031).

mod cache;
mod ddl;
mod load;
mod specs;
mod tables;
mod types;

pub(crate) use cache::CatalogCache;
pub use ddl::{
    add_column, create_index, create_table, declare_entity, declare_membership, drop_entity,
    drop_index, drop_membership, drop_table, set_function_leakproof,
};
pub use load::load_catalog;
pub use specs::{
    AttributeSpec, ColumnSpec, EntitySpec, IndexKey, IndexSpec, MembershipSpec, TableSpec,
};
pub use tables::{ENGINE_PREFIX, is_engine_table};
pub(crate) use tables::{
    PRIVILEGES, create_catalog_tables, read_all, schema_version, stored, text_at,
};
