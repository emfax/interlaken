//! Columns visible to expressions during resolution.

use super::error::ResolveError;
use crate::names::{BindingId, ColumnId, Ident, TableId};
use crate::schema::{OutputColumn, RelSchema};
use crate::types::Type;

/// One column a relation exposes to the operators above it.
#[derive(Clone, Debug)]
pub(crate) struct ScopeColumn {
    /// The binding assigned by the resolver.
    pub(crate) binding: BindingId,
    /// The column within the binding.
    pub(crate) column: ColumnId,
    /// The binding the input IR gave this relation, for remapping bound references.
    pub(crate) original: Option<BindingId>,
    /// The alias qualified references match.
    pub(crate) qualifier: Option<Ident>,
    /// The column name, if it has one.
    pub(crate) name: Option<Ident>,
    /// The column type.
    pub(crate) ty: Type,
    /// Hidden columns come from lowered subqueries and cannot be named.
    pub(crate) hidden: bool,
    /// The table, for columns of a `Get`.
    pub(crate) source: Option<TableId>,
}

/// The columns a relation exposes, in output order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Scope {
    /// The columns, visible and hidden.
    pub(crate) columns: Vec<ScopeColumn>,
}

impl Scope {
    /// Construct a scope of new, unqualified columns under `binding`.
    pub(crate) fn derived(
        binding: BindingId,
        original: Option<BindingId>,
        columns: impl IntoIterator<Item = (Option<Ident>, Type)>,
    ) -> Result<Self, ResolveError> {
        let columns = columns
            .into_iter()
            .enumerate()
            .map(|(i, (name, ty))| {
                Ok(ScopeColumn {
                    binding,
                    column: ColumnId::new(
                        u32::try_from(i)
                            .map_err(|_| ResolveError::Invalid("too many columns".into()))?,
                    ),
                    original,
                    qualifier: None,
                    name,
                    ty,
                    hidden: false,
                    source: None,
                })
            })
            .collect::<Result<_, ResolveError>>()?;
        Ok(Self { columns })
    }

    /// Return the visible columns.
    pub(crate) fn visible(&self) -> impl Iterator<Item = &ScopeColumn> {
        self.columns.iter().filter(|c| !c.hidden)
    }

    /// Return `true` if any column is hidden.
    pub(crate) fn has_hidden(&self) -> bool {
        self.columns.iter().any(|c| c.hidden)
    }

    /// Look up a visible column by name; `Ok(None)` if nothing matches.
    pub(crate) fn lookup_named(
        &self,
        qualifier: Option<&Ident>,
        name: &Ident,
    ) -> Result<Option<&ScopeColumn>, ResolveError> {
        let mut matches = self.visible().filter(|c| {
            c.name.as_ref() == Some(name)
                && qualifier.is_none_or(|q| c.qualifier.as_ref() == Some(q))
        });
        let first = matches.next();
        if first.is_some() && matches.next().is_some() {
            let shown = qualifier.map_or_else(|| name.to_string(), |q| format!("{q}.{name}"));
            return Err(ResolveError::AmbiguousColumn(shown));
        }
        Ok(first)
    }

    /// Look up a column by the binding the input IR gave it.
    pub(crate) fn lookup_bound(
        &self,
        binding: BindingId,
        column: ColumnId,
    ) -> Option<&ScopeColumn> {
        self.columns
            .iter()
            .find(|c| c.original == Some(binding) && c.column == column)
    }

    /// Look up a column by its resolved binding.
    pub(crate) fn lookup_resolved(
        &self,
        binding: BindingId,
        column: ColumnId,
    ) -> Option<&ScopeColumn> {
        self.columns
            .iter()
            .find(|c| c.binding == binding && c.column == column)
    }

    /// Return the scope's visible columns as a public schema.
    pub(crate) fn schema(&self) -> RelSchema {
        RelSchema {
            columns: self
                .visible()
                .map(|c| OutputColumn {
                    binding: c.binding,
                    column: c.column,
                    name: c.name.clone(),
                    ty: c.ty.clone(),
                })
                .collect(),
        }
    }
}
