//! Catalog metadata as the IR sees it (ADR-0013).
//!
//! The resolver, verifier, security lowering, optimizer and reference
//! interpreter read catalog metadata only through the [`Catalog`] trait.
//! [`InMemoryCatalog`] serves IR work and tests without storage; a
//! storage-backed implementation over the `sys_*` tables comes later.

mod builtins;
mod defs;
mod entities;
mod foreign_keys;
mod lookup;
mod memory;

pub use builtins::{
    FIRST_USER_FUNCTION_ID, binary_op_properties, builtin_description, builtin_functions,
    call_optional, cast_properties, install_builtins, unary_op_properties,
};
pub use defs::{
    ColumnDef, FunctionDef, FunctionKind, FunctionProperties, IndexColumn, IndexDef, TableDef,
    Volatility,
};
pub use entities::{AttributeDef, EntityDef, MembershipDef};
pub use foreign_keys::{
    ForeignKeyDef, ReferentialAction, cascade_plan, reachable_cascades, referencing_index,
};
pub use lookup::Catalog;
pub use memory::{CatalogError, InMemoryCatalog};
