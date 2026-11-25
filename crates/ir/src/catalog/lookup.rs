//! The read-only catalog interface every IR stage uses.

use std::sync::Arc;

use super::defs::{FunctionDef, TableDef};
use super::entities::{EntityDef, MembershipDef};
use super::foreign_keys::ForeignKeyDef;
use crate::names::{FunctionId, Ident, QualifiedName, SchemaVersion, TableId};
use crate::types::DataType;

/// Catalog metadata at one schema version (ADR-0013).
///
/// Implementations must be internally consistent: every table, function
/// and entity declaration they return belongs to [`Catalog::schema_version`].
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

    /// Look up an entity type by name (ADR-0017). None by default.
    fn entity_by_name(&self, name: &Ident) -> Option<Arc<EntityDef>> {
        let _ = name;
        None
    }

    /// Look up the entity type backed by `table`. None by default.
    fn entity_for_table(&self, table: TableId) -> Option<Arc<EntityDef>> {
        let _ = table;
        None
    }

    /// Look up the membership of `member` entities in `group` entities. None by default.
    fn membership(&self, member: &Ident, group: &Ident) -> Option<Arc<MembershipDef>> {
        let _ = (member, group);
        None
    }

    /// Return every declared entity type, ordered by name. None by default.
    fn entities(&self) -> Vec<Arc<EntityDef>> {
        Vec::new()
    }

    /// Return the principal types that no table backs, ordered by name
    /// (ADR-0042). None by default.
    fn principal_types(&self) -> Vec<Ident> {
        Vec::new()
    }

    /// Return every membership whose member type is `member`. None by default.
    fn memberships_of(&self, member: &Ident) -> Vec<Arc<MembershipDef>> {
        let _ = member;
        Vec::new()
    }

    /// Return every foreign key, ordered by name (ADR-0035). None by default.
    fn foreign_keys(&self) -> Vec<Arc<ForeignKeyDef>> {
        Vec::new()
    }

    /// Return the foreign keys whose referencing table is `table`, ordered by name.
    fn foreign_keys_of(&self, table: TableId) -> Vec<Arc<ForeignKeyDef>> {
        let mut out = self.foreign_keys();
        out.retain(|fk| fk.table == table);
        out
    }

    /// Return the foreign keys that refer to `table`, ordered by name.
    fn foreign_keys_referencing(&self, table: TableId) -> Vec<Arc<ForeignKeyDef>> {
        let mut out = self.foreign_keys();
        out.retain(|fk| fk.referenced == table);
        out
    }

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
