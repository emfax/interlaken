//! What engine DDL creates, by name.

use interlaken_ir::Type;
pub use interlaken_ir::catalog::ReferentialAction;

/// A column of a new table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSpec {
    /// The column name, unique within its table.
    pub name: String,
    /// The IR type: `(option T)` if the column may hold NULL. Primary key columns never do.
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
    /// For a unique index: `true` if keys containing a NULL never conflict,
    /// as in SQL; `false` if NULLs count as equal, as Postgres's `NULLS NOT
    /// DISTINCT` and option equality (ADR-0034).
    pub nones_distinct: bool,
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

/// A new foreign key (ADR-0035).
///
/// # Examples
///
/// ```
/// use interlaken_engine::catalog::{ForeignKeySpec, ReferentialAction};
///
/// let fk = ForeignKeySpec::new("docs_owner", "docs", ["owner"], "users", ["id"])
///     .on_delete(ReferentialAction::Cascade);
/// assert_eq!(fk.referenced_columns, ["id"]);
/// assert!(!fk.match_full);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeySpec {
    /// The foreign key's name, unique among foreign keys.
    pub name: String,
    /// The referencing table.
    pub table: String,
    /// The referencing columns, in order.
    pub columns: Vec<String>,
    /// The referenced table, which may be `table` itself.
    pub references: String,
    /// The referenced columns, paired with `columns` by position: the
    /// referenced table's primary key or the columns of one of its unique
    /// indexes, in any order.
    pub referenced_columns: Vec<String>,
    /// What deleting a referenced row does to the rows that refer to it.
    pub on_delete: ReferentialAction,
    /// `MATCH FULL`: referencing columns must be all NULL or all present.
    /// Otherwise `MATCH SIMPLE`, where a row with any NULL referencing
    /// column isn't checked.
    pub match_full: bool,
}

impl ForeignKeySpec {
    /// Construct a `NO ACTION`, `MATCH SIMPLE` foreign key from `columns`
    /// of `table` to `referenced_columns` of `references`.
    #[must_use]
    pub fn new<'a>(
        name: impl Into<String>,
        table: impl Into<String>,
        columns: impl IntoIterator<Item = &'a str>,
        references: impl Into<String>,
        referenced_columns: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self {
            name: name.into(),
            table: table.into(),
            columns: columns.into_iter().map(Into::into).collect(),
            references: references.into(),
            referenced_columns: referenced_columns.into_iter().map(Into::into).collect(),
            on_delete: ReferentialAction::NoAction,
            match_full: false,
        }
    }

    /// Return a copy with the action `on_delete`.
    #[must_use]
    pub fn on_delete(mut self, on_delete: ReferentialAction) -> Self {
        self.on_delete = on_delete;
        self
    }

    /// Return a copy that matches `MATCH FULL`.
    #[must_use]
    pub fn match_full(mut self) -> Self {
        self.match_full = true;
        self
    }
}
