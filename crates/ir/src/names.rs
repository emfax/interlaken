//! Names, catalog object IDs and the references IR nodes make to them (ADR-0013).
//!
//! The IR has two forms. Unresolved IR refers to tables, columns and
//! functions by name; resolved IR refers to them by catalog ID and to
//! columns by binding. Each reference type below is an enum with one variant
//! per form, so a single tree type carries both and the resolver (I2) only
//! rewrites the references.

use std::fmt;

/// An identifier: a table, column, function, alias or field name.
///
/// Identifiers are exact; any case folding is the front end's job.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ident(String);

impl Ident {
    /// Construct an identifier from any string.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::Ident;
    ///
    /// assert_eq!(Ident::new("users").as_str(), "users");
    /// ```
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Return the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Ident {
    /// Render the identifier verbatim.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Ident {
    /// Construct an identifier from a string slice.
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl From<String> for Ident {
    /// Construct an identifier from an owned string.
    fn from(name: String) -> Self {
        Self(name)
    }
}

/// A dotted name such as `users` or `public.users`, with at least one part.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QualifiedName(Vec<Ident>);

impl QualifiedName {
    /// Construct a name from its parts, outermost first.
    ///
    /// # Panics
    ///
    /// Panics if `parts` is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::{Ident, QualifiedName};
    ///
    /// let name = QualifiedName::new(vec![Ident::new("public"), Ident::new("users")]);
    /// assert_eq!(name.to_string(), "public.users");
    /// ```
    #[must_use]
    pub fn new(parts: Vec<Ident>) -> Self {
        assert!(
            !parts.is_empty(),
            "a qualified name needs at least one part"
        );
        Self(parts)
    }

    /// Construct a single-part name.
    #[must_use]
    pub fn simple(name: impl Into<Ident>) -> Self {
        Self(vec![name.into()])
    }

    /// Return the parts, outermost first.
    #[must_use]
    pub fn parts(&self) -> &[Ident] {
        &self.0
    }

    /// Return the parts joined with `.`, unquoted: the form storage keeps a
    /// table's name in, such as `app.todos` for a table in namespace `app`
    /// (ADR-0042).
    #[must_use]
    pub fn joined(&self) -> String {
        self.0
            .iter()
            .map(Ident::as_str)
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Return the last (innermost) part.
    #[must_use]
    pub fn last(&self) -> &Ident {
        self.0.last().expect("qualified names are never empty")
    }
}

impl fmt::Display for QualifiedName {
    /// Render the parts joined by dots, without quoting.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, part) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(part.as_str())?;
        }
        Ok(())
    }
}

impl From<&str> for QualifiedName {
    /// Parse a dotted name, splitting on every `.`.
    ///
    /// Use [`QualifiedName::new`] for parts that contain dots.
    fn from(name: &str) -> Self {
        Self(name.split('.').map(Ident::from).collect())
    }
}

/// Declare a `u32` newtype ID with `new` and `get`.
macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            /// Construct the ID from its raw value.
            #[must_use]
            pub const fn new(raw: u32) -> Self {
                Self(raw)
            }

            /// Return the raw value.
            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }
        }
    };
}

id_type!(
    /// A table's catalog ID.
    TableId
);
id_type!(
    /// A function's catalog ID.
    FunctionId
);
id_type!(
    /// A column's position within its relation.
    ///
    /// For a table, this is the catalog column ID: columns are numbered in
    /// declaration order and never renumbered. For a derived relation (a
    /// projection, an aggregate and so on) it is the output position.
    ColumnId
);
id_type!(
    /// Identifies one relation instance within a plan, so columns can be
    /// referenced unambiguously.
    ///
    /// Every operator that introduces new columns (`Get`, `Project`,
    /// `Aggregate`, `Values`, set operations, `Nest`, `Unnest` and the DML
    /// operators) carries a binding in resolved IR. A resolved column
    /// reference names a binding and a column within it. Binding IDs are
    /// unique within a plan.
    BindingId
);

/// A catalog schema version; resolved IR is pinned to one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaVersion(u64);

impl SchemaVersion {
    /// Construct a version from its raw value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A reference to a table.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TableRef {
    /// By name (unresolved).
    Named(QualifiedName),
    /// By catalog ID (resolved).
    Id(TableId),
}

impl From<&str> for TableRef {
    /// Construct a named reference from a dotted name.
    fn from(name: &str) -> Self {
        Self::Named(name.into())
    }
}

impl From<QualifiedName> for TableRef {
    /// Construct a named reference.
    fn from(name: QualifiedName) -> Self {
        Self::Named(name)
    }
}

impl From<TableId> for TableRef {
    /// Construct a resolved reference.
    fn from(id: TableId) -> Self {
        Self::Id(id)
    }
}

/// A reference to a function.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FunctionRef {
    /// By name (unresolved); the resolver picks the overload from argument types.
    Named(QualifiedName),
    /// By catalog ID (resolved).
    Id(FunctionId),
}

impl From<&str> for FunctionRef {
    /// Construct a named reference from a dotted name.
    fn from(name: &str) -> Self {
        Self::Named(name.into())
    }
}

impl From<QualifiedName> for FunctionRef {
    /// Construct a named reference.
    fn from(name: QualifiedName) -> Self {
        Self::Named(name)
    }
}

impl From<FunctionId> for FunctionRef {
    /// Construct a resolved reference.
    fn from(id: FunctionId) -> Self {
        Self::Id(id)
    }
}

/// A reference to a column in an expression.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ColumnRef {
    /// By name, optionally qualified by a relation alias (unresolved).
    Named {
        /// The alias of the relation the column belongs to, if given.
        qualifier: Option<Ident>,
        /// The column name.
        name: Ident,
    },
    /// By binding and position (resolved).
    Bound {
        /// The relation instance.
        binding: BindingId,
        /// The column within it.
        column: ColumnId,
    },
}

impl ColumnRef {
    /// Construct a named reference from `name` or `qualifier.name`.
    ///
    /// Everything before the last `.` is the qualifier.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::{ColumnRef, Ident};
    ///
    /// assert_eq!(
    ///     ColumnRef::named("u.age"),
    ///     ColumnRef::Named { qualifier: Some(Ident::new("u")), name: Ident::new("age") }
    /// );
    /// ```
    #[must_use]
    pub fn named(path: &str) -> Self {
        match path.rsplit_once('.') {
            Some((qualifier, name)) => Self::Named {
                qualifier: Some(qualifier.into()),
                name: name.into(),
            },
            None => Self::Named {
                qualifier: None,
                name: path.into(),
            },
        }
    }

    /// Construct a resolved reference.
    #[must_use]
    pub const fn bound(binding: BindingId, column: ColumnId) -> Self {
        Self::Bound { binding, column }
    }
}

/// A target column of `Insert` or `Update`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TargetColumn {
    /// By name (unresolved).
    Named(Ident),
    /// By catalog column ID (resolved).
    Id(ColumnId),
}

impl From<&str> for TargetColumn {
    /// Construct a named target column.
    fn from(name: &str) -> Self {
        Self::Named(name.into())
    }
}

impl From<ColumnId> for TargetColumn {
    /// Construct a resolved target column.
    fn from(id: ColumnId) -> Self {
        Self::Id(id)
    }
}
