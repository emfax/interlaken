//! Catalog metadata as the IR sees it (ADR-0013).
//!
//! The resolver, verifier, security lowering, optimizer and reference
//! interpreter read catalog metadata only through the [`Catalog`] trait.
//! [`InMemoryCatalog`] serves IR work and tests without storage; a
//! storage-backed implementation over the `sys_*` tables comes later.

mod defs;
mod lookup;
mod memory;

pub use defs::{ColumnDef, FunctionDef, FunctionKind, FunctionProperties, TableDef, Volatility};
pub use lookup::Catalog;
pub use memory::{CatalogError, InMemoryCatalog};
