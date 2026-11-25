//! Resolving unqualified names through a session's search path (ADR-0042).

use std::sync::Arc;

use interlaken_ir::catalog::{
    Catalog, EntityDef, ForeignKeyDef, FunctionDef, MembershipDef, TableDef,
};
use interlaken_ir::{FunctionId, Ident, QualifiedName, SchemaVersion, TableId};

/// The namespace every database has, whose objects are stored unqualified.
pub const PUBLIC: &str = "public";

/// Return the stored form of `name` in namespace `namespace`: unqualified
/// in `public`, `namespace.name` elsewhere.
#[must_use]
pub fn stored_name(namespace: &str, name: &str) -> String {
    if namespace == PUBLIC {
        name.to_owned()
    } else {
        format!("{namespace}.{name}")
    }
}

/// Split a stored name into its namespace and name.
#[must_use]
pub fn split_name(stored: &str) -> (&str, &str) {
    stored.split_once('.').unwrap_or((PUBLIC, stored))
}

/// A catalog that looks up a one-part table name in each namespace of a
/// search path in turn, and `public.name` as `name`; everything else is
/// the inner catalog's.
pub(crate) struct Scoped<'a> {
    /// The catalog.
    pub(crate) inner: &'a dyn Catalog,
    /// The namespaces to search, in order.
    pub(crate) path: &'a [String],
}

impl Catalog for Scoped<'_> {
    /// Return the inner catalog's schema version.
    fn schema_version(&self) -> SchemaVersion {
        self.inner.schema_version()
    }

    /// Look a table up through the search path.
    fn table_by_name(&self, name: &QualifiedName) -> Option<Arc<TableDef>> {
        let parts: Vec<&str> = name.parts().iter().map(Ident::as_str).collect();
        match parts.as_slice() {
            [table] => self.path.iter().find_map(|namespace| {
                self.inner
                    .table_by_name(&QualifiedName::from(stored_name(namespace, table).as_str()))
            }),
            [PUBLIC, table] => self.inner.table_by_name(&QualifiedName::from(*table)),
            _ => self.inner.table_by_name(name),
        }
    }

    /// Look a table up by ID.
    fn table_by_id(&self, id: TableId) -> Option<Arc<TableDef>> {
        self.inner.table_by_id(id)
    }

    /// Look functions up by name; built-in functions are unqualified.
    fn functions_by_name(&self, name: &QualifiedName) -> Vec<Arc<FunctionDef>> {
        self.inner.functions_by_name(name)
    }

    /// Look a function up by ID.
    fn function_by_id(&self, id: FunctionId) -> Option<Arc<FunctionDef>> {
        self.inner.function_by_id(id)
    }

    /// Look an entity type up by its full name.
    fn entity_by_name(&self, name: &Ident) -> Option<Arc<EntityDef>> {
        self.inner.entity_by_name(name)
    }

    /// Look up the entity type backed by `table`.
    fn entity_for_table(&self, table: TableId) -> Option<Arc<EntityDef>> {
        self.inner.entity_for_table(table)
    }

    /// Look a membership up.
    fn membership(&self, member: &Ident, group: &Ident) -> Option<Arc<MembershipDef>> {
        self.inner.membership(member, group)
    }

    /// Return every entity type.
    fn entities(&self) -> Vec<Arc<EntityDef>> {
        self.inner.entities()
    }

    /// Return the principal types with no table.
    fn principal_types(&self) -> Vec<Ident> {
        self.inner.principal_types()
    }

    /// Return the memberships of `member` entities.
    fn memberships_of(&self, member: &Ident) -> Vec<Arc<MembershipDef>> {
        self.inner.memberships_of(member)
    }

    /// Return every foreign key.
    fn foreign_keys(&self) -> Vec<Arc<ForeignKeyDef>> {
        self.inner.foreign_keys()
    }

    /// Return the foreign keys of `table`.
    fn foreign_keys_of(&self, table: TableId) -> Vec<Arc<ForeignKeyDef>> {
        self.inner.foreign_keys_of(table)
    }

    /// Return the foreign keys referring to `table`.
    fn foreign_keys_referencing(&self, table: TableId) -> Vec<Arc<ForeignKeyDef>> {
        self.inner.foreign_keys_referencing(table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert stored names round-trip through their namespace.
    fn names() {
        assert_eq!(stored_name(PUBLIC, "t"), "t");
        assert_eq!(stored_name("app", "t"), "app.t");
        assert_eq!(split_name("t"), (PUBLIC, "t"));
        assert_eq!(split_name("app.t"), ("app", "t"));
    }
}
