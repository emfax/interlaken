//! The storage-backed IR catalog and engine DDL (ADR-0013 phase 3, ADR-0031).

mod cache;
mod ddl;
mod load;
mod namespaces;
mod scoped;
mod specs;
mod tables;
mod types;

pub(crate) use cache::CatalogCache;
pub(crate) use ddl::create_principal_type;
pub use ddl::{
    add_column, create_foreign_key, create_index, create_table, declare_entity, declare_membership,
    declare_principal_type, drop_entity, drop_foreign_key, drop_index, drop_membership,
    drop_principal_type, drop_table, set_function_leakproof,
};
pub use load::load_catalog;
pub(crate) use namespaces::create_namespaces_table;
pub use namespaces::{
    ObjectKind, create_namespace, drop_namespace, namespace_exists, namespace_of, owns,
    qualify_new, resolve_existing, set_namespace_owner,
};
pub(crate) use scoped::Scoped;
pub use scoped::{PUBLIC, split_name, stored_name};
pub use specs::{
    AttributeSpec, ColumnSpec, EntitySpec, ForeignKeySpec, IndexKey, IndexSpec, MembershipSpec,
    ReferentialAction, TableSpec,
};
pub use tables::{ENGINE_PREFIX, catalog_table_description, is_engine_table};
pub(crate) use tables::{
    PRIVILEGES, create_catalog_tables, read_all, schema_version, stored, text_at,
};
