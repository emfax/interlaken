//! Table, column and index definitions, and the specifications DDL takes.

use crate::encoding::{ColumnType, SortOrder};

/// Identifies a table. User tables start at [`TableId::FIRST_USER`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableId(pub(crate) i32);

impl TableId {
    /// The first ID handed to user objects; 1–99 are reserved for system tables.
    pub const FIRST_USER: i32 = 100;

    /// Return the raw ID, as stored in keys.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

/// Identifies an index. Index 0 of every table is its primary row storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IndexId(pub(crate) i32);

impl IndexId {
    /// The pseudo-index holding a table's rows, keyed by primary key.
    pub const PRIMARY: Self = Self(0);

    /// Return the raw ID, as stored in keys.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

/// A column: name, type and nullability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    /// Column name, unique within its table.
    pub name: String,
    /// Column type.
    pub ty: ColumnType,
    /// Whether the column accepts NULL.
    pub nullable: bool,
}

impl ColumnDef {
    /// Construct a nullable column.
    #[must_use]
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable: true,
        }
    }

    /// Return the column marked NOT NULL.
    #[must_use]
    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }
}

/// One column of an index key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexColumn {
    /// Position of the column in its table.
    pub column: usize,
    /// Direction and NULL placement in the index.
    pub order: SortOrder,
}

/// A secondary index, as stored in the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexDef {
    pub(crate) id: IndexId,
    pub(crate) name: String,
    pub(crate) columns: Vec<IndexColumn>,
    pub(crate) unique: bool,
}

impl IndexDef {
    /// Return the index ID.
    #[must_use]
    pub fn id(&self) -> IndexId {
        self.id
    }

    /// Return the index name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the indexed columns in key order.
    #[must_use]
    pub fn columns(&self) -> &[IndexColumn] {
        &self.columns
    }

    /// Return `true` for a unique index.
    #[must_use]
    pub fn is_unique(&self) -> bool {
        self.unique
    }
}

/// A table definition, read from the catalog at a transaction's snapshot.
///
/// Only the catalog constructs these. Use a definition only in the
/// transaction that read it: the row layer registers a dependency on the
/// definition, so a concurrent DDL change aborts the transaction instead of
/// leaving rows and indexes out of step (ADR-0007).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableDef {
    pub(crate) id: TableId,
    pub(crate) name: String,
    pub(crate) schema_version: u32,
    pub(crate) columns: Vec<ColumnDef>,
    pub(crate) primary_key: Vec<usize>,
    pub(crate) indexes: Vec<IndexDef>,
}

impl TableDef {
    /// Return the table ID.
    #[must_use]
    pub fn id(&self) -> TableId {
        self.id
    }

    /// Return the table name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the schema version, incremented by every DDL change to the table.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Return the columns in order.
    #[must_use]
    pub fn columns(&self) -> &[ColumnDef] {
        &self.columns
    }

    /// Return the positions of the primary key columns, in key order.
    #[must_use]
    pub fn primary_key(&self) -> &[usize] {
        &self.primary_key
    }

    /// Return the secondary indexes.
    #[must_use]
    pub fn indexes(&self) -> &[IndexDef] {
        &self.indexes
    }

    /// Return the position of the column called `name`.
    #[must_use]
    pub fn column_position(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Return the index called `name`.
    #[must_use]
    pub fn index(&self, name: &str) -> Option<&IndexDef> {
        self.indexes.iter().find(|i| i.name == name)
    }

    /// Return the column types in order.
    pub(crate) fn types(&self) -> Vec<ColumnType> {
        self.columns.iter().map(|c| c.ty).collect()
    }
}

/// What [`create_table`](crate::catalog::create_table) creates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSpec {
    /// Table name. Names starting with `sys_` are reserved.
    pub name: String,
    /// Columns in order. Primary key columns are made NOT NULL.
    pub columns: Vec<ColumnDef>,
    /// Names of the primary key columns, in key order.
    pub primary_key: Vec<String>,
}

/// What [`create_index`](crate::catalog::create_index) creates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSpec {
    /// Index name, unique across the database.
    pub name: String,
    /// Name of the indexed table.
    pub table: String,
    /// Indexed columns in key order, with their sort orders.
    pub columns: Vec<(String, SortOrder)>,
    /// Reject rows whose indexed values (all non-NULL) duplicate another row's.
    pub unique: bool,
}
