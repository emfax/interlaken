//! Entity and membership declarations (ADR-0017).
//!
//! Entity types are declared separately from table DDL: each maps an
//! entity type to its table, key column and attribute columns.
//! Memberships declare which entity types can be members of which (users
//! in groups, folders in folders), with an edges table of direct
//! memberships and a closure table of every transitive pair, so the
//! hierarchy test `in` is one indexed semi-join.

use crate::names::{ColumnId, Ident, TableId};

/// One attribute of an entity type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributeDef {
    /// The attribute name policies use.
    pub name: Ident,
    /// The column holding it; NULL means the attribute is absent.
    pub column: ColumnId,
    /// The entity type the column's value refers to, for foreign keys.
    pub references: Option<Ident>,
}

/// An entity type backed by a table: each row is an entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityDef {
    /// The entity type's name, such as `User`.
    pub name: Ident,
    /// The table holding the entities.
    pub table: TableId,
    /// The column identifying each entity (the entity ID).
    pub key: ColumnId,
    /// The attributes policies can read.
    pub attributes: Vec<AttributeDef>,
}

impl EntityDef {
    /// Look up an attribute by name.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&AttributeDef> {
        self.attributes.iter().find(|a| a.name.as_str() == name)
    }
}

/// Entities of type `member` can belong to entities of type `group`,
/// directly (rows of `edges`) or transitively (rows of `closure`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipDef {
    /// The member entity type.
    pub member: Ident,
    /// The group entity type.
    pub group: Ident,
    /// The table of direct memberships.
    pub edges: TableId,
    /// The columns of `edges`: member key, group key.
    pub edge_columns: (ColumnId, ColumnId),
    /// The table of transitive memberships, maintained with every edge change.
    pub closure: TableId,
    /// The columns of `closure`: descendant key, ancestor key, number of paths.
    pub closure_columns: (ColumnId, ColumnId, ColumnId),
}
