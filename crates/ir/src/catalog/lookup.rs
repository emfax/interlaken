//! The read-only catalog interface every IR stage uses.

use std::sync::Arc;

use super::defs::{FunctionDef, TableDef};
use crate::names::{FunctionId, QualifiedName, SchemaVersion, TableId};
use crate::types::DataType;

/// Catalog metadata at one schema version (ADR-0013).
///
/// Implementations must be internally consistent: every table and
/// function they return belongs to [`Catalog::schema_version`]. Entity
/// lookups (ADR-0017) join this trait with the security core in I4.
pub trait Catalog: Send + Sync {
    /// Return the schema version this view of the catalog describes.
    fn schema_version(&self) -> SchemaVersion;

    /// Look up a table by its exact name.
    fn table_by_name(&self, name: &QualifiedName) -> Option<Arc<TableDef>>;

    /// Look up a table by ID.
    fn table_by_id(&self, id: TableId) -> Option<Arc<TableDef>>;

    /// Return every overload of the function with this exact name, in ID order.
    fn functions_by_name(&self, name: &QualifiedName) -> Vec<Arc<FunctionDef>>;

    /// Look up a function overload by ID.
    fn function_by_id(&self, id: FunctionId) -> Option<Arc<FunctionDef>>;

    /// Look up the overload of `name` whose argument types are exactly `args`.
    ///
    /// Implicit casts are the resolver's job; this matches exactly.
    fn function_by_name(
        &self,
        name: &QualifiedName,
        args: &[DataType],
    ) -> Option<Arc<FunctionDef>> {
        self.functions_by_name(name)
            .into_iter()
            .find(|f| f.args == args)
    }
}
