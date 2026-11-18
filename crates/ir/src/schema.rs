//! The output columns of a resolved relation.

use crate::names::{BindingId, ColumnId, Ident};
use crate::types::Type;

/// One output column of a relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputColumn {
    /// The relation instance the column belongs to.
    pub binding: BindingId,
    /// The column within it.
    pub column: ColumnId,
    /// The column's name, if it has one.
    pub name: Option<Ident>,
    /// The column's type.
    pub ty: Type,
}

/// The output columns of a relation, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelSchema {
    /// The columns.
    pub columns: Vec<OutputColumn>,
}
