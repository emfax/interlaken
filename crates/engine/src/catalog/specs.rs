//! What engine DDL creates, by name.

use interlaken_ir::Type;

/// A column of a new table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSpec {
    /// The column name, unique within its table.
    pub name: String,
    /// The IR type, including nullability. Primary key columns become NOT NULL.
    pub ty: Type,
}

impl ColumnSpec {
    /// Construct a column.
    #[must_use]
    pub fn new(name: impl Into<String>, ty: Type) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}

/// A new table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSpec {
    /// The table name. Names starting with `sys_` or `engine_` are reserved.
    pub name: String,
    /// The columns, in order.
    pub columns: Vec<ColumnSpec>,
    /// The primary key columns, by name, in key order.
    pub primary_key: Vec<String>,
}

/// One key column of a new index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexKey {
    /// The column name.
    pub column: String,
    /// `true` for descending order.
    pub descending: bool,
    /// `true` if NULLs sort before other values.
    pub nulls_first: bool,
}

impl IndexKey {
    /// Construct an ascending key column, NULLs last (the Postgres default).
    #[must_use]
    pub fn asc(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            descending: false,
            nulls_first: false,
        }
    }

    /// Construct a descending key column, NULLs first (the Postgres default).
    #[must_use]
    pub fn desc(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            descending: true,
            nulls_first: true,
        }
    }
}

/// A new secondary index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSpec {
    /// The index name, unique across the database.
    pub name: String,
    /// The indexed table.
    pub table: String,
    /// The key columns, in key order.
    pub columns: Vec<IndexKey>,
    /// Reject rows whose key (all non-NULL) duplicates another row's.
    pub unique: bool,
}

/// One attribute of a new entity type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributeSpec {
    /// The attribute name policies use.
    pub name: String,
    /// The column holding it.
    pub column: String,
    /// The entity type the column's value refers to, for foreign keys.
    pub references: Option<String>,
}

impl AttributeSpec {
    /// Construct an attribute held in the column of the same name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            column: name.clone(),
            name,
            references: None,
        }
    }

    /// Construct an attribute held in `column`, referring to entities of type `entity`.
    #[must_use]
    pub fn reference(
        name: impl Into<String>,
        column: impl Into<String>,
        entity: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            column: column.into(),
            references: Some(entity.into()),
        }
    }
}

/// A new entity type backed by a table (ADR-0017).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntitySpec {
    /// The entity type's name, such as `User`.
    pub name: String,
    /// The table holding the entities.
    pub table: String,
    /// The column identifying each entity.
    pub key: String,
    /// The attributes policies can read.
    pub attributes: Vec<AttributeSpec>,
}

/// A new membership between entity types (ADR-0017).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipSpec {
    /// The member entity type.
    pub member: String,
    /// The group entity type.
    pub group: String,
    /// The table of direct memberships.
    pub edges: String,
    /// The columns of `edges`: member key, group key.
    pub edge_columns: (String, String),
    /// The table of transitive memberships.
    pub closure: String,
    /// The columns of `closure`: descendant key, ancestor key, number of paths.
    pub closure_columns: (String, String, String),
}
