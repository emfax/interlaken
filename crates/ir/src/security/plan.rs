//! Security lowering of plans (ADR-0015, ADR-0018).
//!
//! Every `Get` of a governed table (one backing a declared entity type)
//! gets a `PolicyFilter` with the read residual for the principal, so the
//! rest of the plan only sees rows the principal may read. The target rows
//! of an update or delete must also pass that action's policy (Postgres
//! `USING`), and every row an insert or update writes must pass a
//! `WriteCheck` (Postgres `WITH CHECK`), which fails the statement
//! otherwise. Tables that back no entity type are not governed.
//!
//! The lowered plan is resolved again, which lowers the policies'
//! subqueries to applies below each barrier, and verified as trusted.

use std::collections::HashMap;

use super::error::SecurityError;
use super::eval::EntityStore;
use super::expr::{Action, EntityUid};
use super::lower::Lowerer;
use super::provider::{PartialRequest, PolicyProvider, Residual, SecurityContext};
use crate::catalog::Catalog;
use crate::expr::Expr;
use crate::names::{BindingId, ColumnId, ColumnRef, TableId, TableRef, TargetColumn};
use crate::rel::{Plan, Rel};
use crate::value::Value;

/// State of one lowering pass.
struct Lowering<'a> {
    /// The catalog.
    catalog: &'a dyn Catalog,
    /// The policies.
    provider: &'a dyn PolicyProvider,
    /// Who is asking.
    context: &'a SecurityContext,
    /// Entity data for partial evaluation.
    store: &'a dyn EntityStore,
    /// The principal followed by its ancestors.
    principal_and_ancestors: Vec<EntityUid>,
    /// Residuals computed so far.
    residuals: HashMap<(Action, TableId), Residual>,
    /// Counter for subquery aliases, shared across residuals.
    next_alias: u32,
}

/// Find the binding of the first `Get` of `table` in `rel`.
fn find_get(rel: &Rel, table: TableId) -> Option<BindingId> {
    if let Rel::Get {
        table: TableRef::Id(t),
        binding,
        ..
    } = rel
        && *t == table
    {
        return *binding;
    }
    rel.inputs().into_iter().find_map(|i| find_get(i, table))
}

impl Lowering<'_> {
    /// Return the residual for `action` on `table`.
    fn residual(
        &mut self,
        action: Action,
        table: TableId,
        resource_type: &crate::names::Ident,
    ) -> Result<Residual, SecurityError> {
        if let Some(r) = self.residuals.get(&(action, table)) {
            return Ok(r.clone());
        }
        let req = PartialRequest {
            context: self.context,
            action,
            resource_type,
        };
        let residual = self.provider.partial_eval(&req, self.store)?;
        self.residuals.insert((action, table), residual.clone());
        Ok(residual)
    }

    /// Return the condition that `actions` are all allowed on the row given by `resource`;
    /// `None` if they always are.
    fn condition(
        &mut self,
        table: TableId,
        actions: &[Action],
        resource: &dyn Fn(ColumnId) -> Expr,
    ) -> Result<Option<Expr>, SecurityError> {
        let Some(entity) = self.catalog.entity_for_table(table) else {
            return Ok(None);
        };
        let table_def = self
            .catalog
            .table_by_id(table)
            .ok_or_else(|| SecurityError::Undeclared(format!("table {}", table.get())))?;
        let mut parts = Vec::new();
        for &action in actions {
            match self.residual(action, table, &entity.name)? {
                Residual::Allow => {}
                Residual::Deny => return Ok(Some(Expr::Literal(Value::Bool(false)))),
                Residual::Conditional(e) => {
                    let mut lowerer = Lowerer {
                        catalog: self.catalog,
                        entity: entity.clone(),
                        table: table_def.clone(),
                        resource,
                        principal_and_ancestors: self.principal_and_ancestors.clone(),
                        next_alias: self.next_alias,
                    };
                    parts.push(lowerer.condition(&e)?);
                    self.next_alias = lowerer.next_alias;
                }
            }
        }
        Ok(match parts.len() {
            0 => None,
            1 => parts.pop(),
            _ => Some(Expr::And(parts)),
        })
    }

    /// Lower `rel`; `dml` names the table an enclosing update or delete changes, and its action.
    fn rel(&mut self, rel: Rel, dml: Option<(TableId, Action)>) -> Result<Rel, SecurityError> {
        Ok(match rel {
            Rel::Get {
                table: TableRef::Id(t),
                alias,
                binding: Some(b),
            } => {
                let mut actions = vec![Action::Read];
                if let Some((target, action)) = dml
                    && target == t
                {
                    actions.push(action);
                }
                let get = Rel::Get {
                    table: TableRef::Id(t),
                    alias,
                    binding: Some(b),
                };
                let resource = |c: ColumnId| Expr::Column(ColumnRef::bound(b, c));
                match self.condition(t, &actions, &resource)? {
                    None => get,
                    Some(residual) => Rel::PolicyFilter {
                        input: Box::new(get),
                        residual,
                    },
                }
            }
            Rel::Insert {
                target,
                columns,
                input,
                returning,
                binding,
            } => {
                let input = self.rel(*input, None)?;
                let TableRef::Id(t) = target.table else {
                    return Err(SecurityError::Invalid("unresolved insert target".into()));
                };
                let outputs = crate::optimize::output_columns(&input, self.catalog);
                let table = self.catalog.table_by_id(t);
                let resource = |c: ColumnId| -> Expr {
                    match columns.iter().position(|tc| *tc == TargetColumn::Id(c)) {
                        Some(i) => outputs
                            .get(i)
                            .map_or(Expr::Literal(Value::Null), |&(b, col)| {
                                Expr::Column(ColumnRef::bound(b, col))
                            }),
                        None => Expr::Cast {
                            expr: Box::new(Expr::Literal(Value::Null)),
                            to: table.as_ref().and_then(|t| t.column_by_id(c)).map_or(
                                crate::types::DataType::Scalar(crate::types::ScalarType::Text),
                                |c| c.ty.data.clone(),
                            ),
                        },
                    }
                };
                let input = match self.condition(t, &[Action::Insert], &resource)? {
                    None => input,
                    Some(check) => Rel::WriteCheck {
                        input: Box::new(input),
                        check,
                    },
                };
                Rel::Insert {
                    target,
                    columns,
                    input: Box::new(input),
                    returning,
                    binding,
                }
            }
            Rel::Update {
                target,
                input,
                assignments,
                returning,
                binding,
            } => {
                let TableRef::Id(t) = target.table else {
                    return Err(SecurityError::Invalid("unresolved update target".into()));
                };
                let input = self.rel(*input, Some((t, Action::Update)))?;
                let source = find_get(&input, t).ok_or_else(|| {
                    SecurityError::Invalid("update input does not read its target".into())
                })?;
                let new_row = |c: ColumnId| -> Expr {
                    assignments
                        .iter()
                        .find(|a| a.column == TargetColumn::Id(c))
                        .map_or_else(
                            || Expr::Column(ColumnRef::bound(source, c)),
                            |a| a.value.clone(),
                        )
                };
                let input = match self.condition(t, &[Action::Update], &new_row)? {
                    None => input,
                    Some(check) => Rel::WriteCheck {
                        input: Box::new(input),
                        check,
                    },
                };
                Rel::Update {
                    target,
                    input: Box::new(input),
                    assignments,
                    returning,
                    binding,
                }
            }
            Rel::Delete {
                target,
                input,
                returning,
                binding,
            } => {
                let TableRef::Id(t) = target.table else {
                    return Err(SecurityError::Invalid("unresolved delete target".into()));
                };
                let input = self.rel(*input, Some((t, Action::Delete)))?;
                Rel::Delete {
                    target,
                    input: Box::new(input),
                    returning,
                    binding,
                }
            }
            mut other => {
                for input in other.inputs_mut() {
                    let child = crate::optimize::take(input);
                    *input = self.rel(child, dml)?;
                }
                other
            }
        })
    }
}

/// Fail if `plan` reads a column of a governed table the principal may not read.
///
/// A column counts as read if any expression references it, or if the
/// whole row of its relation is consumed by position: the plan's output,
/// a set operation, an insert or `DISTINCT`. Policy residuals are added
/// later and are not checked: the engine may read any column to enforce
/// policies.
fn check_columns(
    plan: &Plan,
    catalog: &dyn Catalog,
    provider: &dyn PolicyProvider,
    context: &SecurityContext,
    store: &dyn EntityStore,
) -> Result<(), SecurityError> {
    let mut read = std::collections::HashSet::new();
    crate::optimize::referenced(&plan.root, &mut read);
    let mut whole_rows = std::collections::HashSet::new();
    crate::optimize::positional(&plan.root, true, &mut whole_rows);
    /// Collect the table and binding of every get and DML target row.
    fn row_sources(rel: &Rel, out: &mut Vec<(TableId, BindingId)>) {
        match rel {
            Rel::Get {
                table: TableRef::Id(t),
                binding: Some(b),
                ..
            } => out.push((*t, *b)),
            Rel::Insert { target, .. }
            | Rel::Update { target, .. }
            | Rel::Delete { target, .. } => {
                if let (TableRef::Id(t), Some(b)) = (&target.table, target.binding) {
                    out.push((*t, b));
                }
            }
            _ => {}
        }
        rel.inputs().into_iter().for_each(|i| row_sources(i, out));
    }
    let mut sources = Vec::new();
    row_sources(&plan.root, &mut sources);
    for (table, binding) in sources {
        let (Some(entity), Some(def)) =
            (catalog.entity_for_table(table), catalog.table_by_id(table))
        else {
            continue;
        };
        for attribute in &entity.attributes {
            let used = whole_rows.contains(&binding) || read.contains(&(binding, attribute.column));
            if used
                && provider.column_access(context, &entity.name, &attribute.name, store)?
                    == super::provider::Decision::Deny
            {
                return Err(SecurityError::ColumnDenied {
                    table: def.name.to_string(),
                    column: attribute.name.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Apply `provider`'s policies for `context` to a resolved, verified plan.
///
/// Returns a resolved plan with `PolicyFilter` and `WriteCheck` operators,
/// verified as trusted, ready for the optimizer. Fails closed: if any
/// policy cannot be lowered, the query does not run.
///
/// # Errors
///
/// Returns [`SecurityError::ColumnDenied`] if the plan reads a column the
/// principal may not read, or another [`SecurityError`] if a policy uses
/// something the catalog does not declare or lowering cannot express, or
/// the secured plan is invalid.
pub fn secure(
    plan: &Plan,
    catalog: &dyn Catalog,
    provider: &dyn PolicyProvider,
    context: &SecurityContext,
    store: &dyn EntityStore,
) -> Result<Plan, SecurityError> {
    check_columns(plan, catalog, provider, context, store)?;
    let mut principal_and_ancestors = vec![context.principal.clone()];
    principal_and_ancestors.extend(store.ancestors(&context.principal));
    let mut lowering = Lowering {
        catalog,
        provider,
        context,
        store,
        principal_and_ancestors,
        residuals: HashMap::new(),
        next_alias: 0,
    };
    let root = lowering.rel(plan.root.clone(), None)?;
    let lowered = Plan {
        schema_version: plan.schema_version,
        root,
    };
    let resolved = crate::resolve::resolve(&lowered, catalog)
        .map_err(|e| SecurityError::Invalid(e.to_string()))?;
    crate::verify::verify(
        &resolved.plan,
        catalog,
        &crate::verify::VerifyOptions { trusted: true },
    )
    .map_err(|e| SecurityError::Invalid(e.to_string()))?;
    Ok(resolved.plan)
}
