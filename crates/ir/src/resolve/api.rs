//! The resolver's entry point and state.

use std::collections::HashMap;

use super::error::ResolveError;
use super::scope::Scope;
use crate::catalog::Catalog;
use crate::expr::Expr;
use crate::expr::NamedExpr;
use crate::names::{BindingId, ColumnRef};
use crate::rel::{Plan, Rel};
use crate::schema::RelSchema;
use crate::types::Type;

/// A resolved plan and its output columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The resolved plan, pinned to the catalog's schema version.
    pub plan: Plan,
    /// The plan's output columns.
    pub schema: RelSchema,
}

/// Resolve a plan against `catalog`: names to IDs and bindings, implicit
/// conversions to explicit casts, subqueries to `Apply`.
///
/// The input may be unresolved, partly resolved or resolved; bound column
/// references in it are remapped to the new bindings. The result should
/// still go through [`verify`](crate::verify::verify) before it runs.
///
/// # Errors
///
/// Returns a [`ResolveError`] for unknown or ambiguous names, type errors,
/// and constructs the resolver does not support.
///
/// # Examples
///
/// ```
/// use interlaken_ir::catalog::{InMemoryCatalog, TableDef};
/// use interlaken_ir::resolve::resolve;
/// use interlaken_ir::text::{parse_plan, print_plan};
/// use interlaken_ir::{DataType, ScalarType, SchemaVersion, TableId};
///
/// let mut catalog = InMemoryCatalog::new(SchemaVersion::new(3));
/// catalog.add_table(
///     TableDef::new(TableId::new(100), "users")
///         .column("age", DataType::from(ScalarType::Int4).nullable()),
/// )?;
/// let plan = parse_plan("(plan (filter (get users :as u) (> u.age 18)))")?;
/// let resolved = resolve(&plan, &catalog)?;
/// assert_eq!(
///     print_plan(&resolved.plan),
///     "(plan :schema-version 3\n  (filter (get (tid 100) :as u :bind 1) (> #1.0 (int4 18))))"
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn resolve(plan: &Plan, catalog: &dyn Catalog) -> Result<Resolved, ResolveError> {
    let mut resolver = Resolver {
        catalog,
        next_binding: 1,
        params: HashMap::new(),
    };
    let (root, scope) = resolver.rel(&plan.root, &[])?;
    let (root, scope) = resolver.visible_only(root, scope)?;
    Ok(Resolved {
        plan: Plan {
            schema_version: Some(catalog.schema_version()),
            root,
        },
        schema: scope.schema(),
    })
}

/// The resolver's state while resolving one plan.
pub(crate) struct Resolver<'c> {
    /// The catalog names are resolved against.
    pub(crate) catalog: &'c dyn Catalog,
    /// The next binding to assign.
    next_binding: u32,
    /// The type of each parameter seen so far.
    params: HashMap<u32, Type>,
}

impl Resolver<'_> {
    /// Allocate a fresh binding.
    pub(crate) fn fresh(&mut self) -> Result<BindingId, ResolveError> {
        let binding = BindingId::new(self.next_binding);
        self.next_binding = self
            .next_binding
            .checked_add(1)
            .ok_or_else(|| ResolveError::Invalid("too many bindings".into()))?;
        Ok(binding)
    }

    /// Record parameter `index`'s type, which must agree with earlier uses.
    pub(crate) fn param(&mut self, index: u32, ty: &Type) -> Result<(), ResolveError> {
        if index == 0 {
            return Err(ResolveError::Invalid(
                "parameters are numbered from 1".into(),
            ));
        }
        match self.params.get(&index) {
            Some(prev) if prev != ty => Err(ResolveError::Type(format!(
                "parameter {index} is used as both {prev} and {ty}"
            ))),
            Some(_) => Ok(()),
            None => {
                self.params.insert(index, ty.clone());
                Ok(())
            }
        }
    }

    /// Return `rel` unchanged if it has no hidden columns, or projected to its visible ones.
    ///
    /// Positional consumers (the plan's output, set operations, inserts)
    /// must not see the columns of lowered subqueries.
    pub(crate) fn visible_only(
        &mut self,
        rel: Rel,
        scope: Scope,
    ) -> Result<(Rel, Scope), ResolveError> {
        if !scope.has_hidden() {
            return Ok((rel, scope));
        }
        let binding = self.fresh()?;
        let visible: Vec<_> = scope.visible().cloned().collect();
        let exprs = visible
            .iter()
            .map(|c| NamedExpr {
                expr: Expr::Column(ColumnRef::bound(c.binding, c.column)),
                name: c.name.clone(),
            })
            .collect();
        let scope = Scope::derived(binding, None, visible.into_iter().map(|c| (c.name, c.ty)))?;
        Ok((
            Rel::Project {
                input: Box::new(rel),
                exprs,
                binding: Some(binding),
            },
            scope,
        ))
    }
}
