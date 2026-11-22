//! Table and function definitions.

use crate::names::{ColumnId, FunctionId, Ident, QualifiedName, TableId};
use crate::types::{DataType, Type};

/// A column of a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    /// The column's ID: its position in declaration order.
    pub id: ColumnId,
    /// The column name.
    pub name: Ident,
    /// The column type: optional if the column may hold NULL.
    pub ty: Type,
}

/// One column of an index key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexColumn {
    /// The column.
    pub column: ColumnId,
    /// `true` for descending order.
    pub descending: bool,
    /// `true` if NULLs sort before other values.
    pub nulls_first: bool,
}

impl IndexColumn {
    /// Construct an ascending key column, NULLs last (the Postgres default).
    #[must_use]
    pub const fn asc(column: ColumnId) -> Self {
        Self {
            column,
            descending: false,
            nulls_first: false,
        }
    }

    /// Construct a descending key column, NULLs first (the Postgres default).
    #[must_use]
    pub const fn desc(column: ColumnId) -> Self {
        Self {
            column,
            descending: true,
            nulls_first: true,
        }
    }
}

/// A secondary index, which the physical planner may use as an access path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IndexDef {
    /// The index name, which the storage catalog knows it by.
    pub name: Ident,
    /// The key columns, in key order.
    pub columns: Vec<IndexColumn>,
    /// Whether the index is unique: at most one row per key with no NULLs.
    pub unique: bool,
    /// For a unique index: `true` if keys containing a NULL never conflict,
    /// `false` if NULLs count as equal (ADR-0034).
    pub nones_distinct: bool,
}

/// A table's definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableDef {
    /// The table's catalog ID.
    pub id: TableId,
    /// The table's name.
    pub name: QualifiedName,
    /// The columns, in ID order.
    pub columns: Vec<ColumnDef>,
    /// The primary key columns, in key order.
    pub primary_key: Vec<ColumnId>,
    /// Whether this is a system table, which clients may not write.
    pub system: bool,
    /// The secondary indexes.
    pub indexes: Vec<IndexDef>,
}

impl TableDef {
    /// Start a definition with no columns.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::catalog::TableDef;
    /// use interlaken_ir::{DataType, ScalarType, TableId};
    ///
    /// let users = TableDef::new(TableId::new(100), "users")
    ///     .column("id", DataType::from(ScalarType::Int8).plain())
    ///     .column("name", DataType::from(ScalarType::Text).option())
    ///     .primary_key(["id"]);
    /// assert_eq!(users.column_by_name("name").unwrap().id.get(), 1);
    /// ```
    #[must_use]
    pub fn new(id: TableId, name: impl Into<QualifiedName>) -> Self {
        Self {
            id,
            name: name.into(),
            columns: Vec::new(),
            primary_key: Vec::new(),
            system: false,
            indexes: Vec::new(),
        }
    }

    /// Mark the table as a system table, which clients may not write.
    #[must_use]
    pub fn system(mut self) -> Self {
        self.system = true;
        self
    }

    /// Append a column, numbered after the existing ones.
    ///
    /// # Panics
    ///
    /// Panics if the table already has `u32::MAX` columns.
    #[must_use]
    pub fn column(mut self, name: impl Into<Ident>, ty: Type) -> Self {
        let id = u32::try_from(self.columns.len()).expect("too many columns");
        self.columns.push(ColumnDef {
            id: ColumnId::new(id),
            name: name.into(),
            ty,
        });
        self
    }

    /// Set the primary key by column name.
    ///
    /// # Panics
    ///
    /// Panics if a name is not a column of the table.
    #[must_use]
    pub fn primary_key<'a>(mut self, names: impl IntoIterator<Item = &'a str>) -> Self {
        self.primary_key = names
            .into_iter()
            .map(|name| {
                self.column_by_name(name)
                    .unwrap_or_else(|| panic!("no column {name} in {}", self.name))
                    .id
            })
            .collect();
        self
    }

    /// Add a secondary index on ascending columns, by name.
    ///
    /// # Panics
    ///
    /// Panics if a name is not a column of the table.
    #[must_use]
    pub fn index<'a>(
        mut self,
        name: impl Into<Ident>,
        columns: impl IntoIterator<Item = &'a str>,
        unique: bool,
    ) -> Self {
        let columns = columns
            .into_iter()
            .map(|c| {
                IndexColumn::asc(
                    self.column_by_name(c)
                        .unwrap_or_else(|| panic!("no column {c} in {}", self.name))
                        .id,
                )
            })
            .collect();
        self.indexes.push(IndexDef {
            name: name.into(),
            columns,
            unique,
            nones_distinct: true,
        });
        self
    }

    /// Look up a column by name.
    #[must_use]
    pub fn column_by_name(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name.as_str() == name)
    }

    /// Look up a column by ID.
    #[must_use]
    pub fn column_by_id(&self, id: ColumnId) -> Option<&ColumnDef> {
        self.columns.get(usize::try_from(id.get()).ok()?)
    }
}

/// Whether a function's result can change for the same arguments (ADR-0011).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Volatility {
    /// Same result for the same arguments, always: may be constant-folded.
    Immutable,
    /// Same result within one statement.
    Stable,
    /// May change on every call, or has side effects.
    Volatile,
}

/// The properties the optimizer and security lowering rely on (ADR-0011).
///
/// They come from the catalog only, never from client-supplied IR.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionProperties {
    /// Folding, reordering and caching rules.
    pub volatility: Volatility,
    /// NULL in any argument gives NULL out without calling the function.
    pub strict: bool,
    /// Cannot reveal its arguments through errors or side effects, so it may
    /// move below a security barrier. Only administrators may set this.
    pub leakproof: bool,
    /// Gives the same result when a transaction is retried.
    pub deterministic: bool,
    /// Estimated cost per call, in units of a simple comparison.
    pub cost: f64,
    /// Estimated fraction of rows for which a boolean result is TRUE, if known.
    pub selectivity: Option<f64>,
}

impl Default for FunctionProperties {
    /// Return the most conservative properties: volatile, not strict, not
    /// leakproof, not deterministic, cost 1.
    fn default() -> Self {
        Self {
            volatility: Volatility::Volatile,
            strict: false,
            leakproof: false,
            deterministic: false,
            cost: 1.0,
            selectivity: None,
        }
    }
}

/// Whether a function is called per row or aggregates groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FunctionKind {
    /// Called once per row; allowed in any expression.
    Scalar,
    /// Folds a group of rows; allowed only in `Aggregate`.
    Aggregate,
}

/// One function overload's definition.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDef {
    /// The overload's catalog ID.
    pub id: FunctionId,
    /// The function's name, shared by its overloads.
    pub name: QualifiedName,
    /// Scalar or aggregate.
    pub kind: FunctionKind,
    /// The argument types.
    pub args: Vec<DataType>,
    /// The result type. For a strict function the result is also NULL
    /// whenever an argument is; an aggregate's result type says whether it
    /// can be NULL (`count` cannot, `sum` over no rows is).
    pub returns: Type,
    /// The declared properties.
    pub properties: FunctionProperties,
}
