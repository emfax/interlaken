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
//! A column whose column policies depend on the row and declare a mask
//! (ADR-0032) is masked where it is read: the governed `Get` is wrapped in
//! a projection that passes every column through except masked ones,
//! which become `policy_mask(visible, value, mask(value))`. The projection
//! keeps the `Get`'s binding, so everything above reads masked values.
//! `policy_mask` is volatile, so the optimizer never moves the condition
//! out of the projection, and `EXPLAIN` hides it by name.
//!
//! Masks also govern writes, which read through them like queries do,
//! while the row written is never a masked one:
//!
//! - **Carried target rows.** An update or delete carries the whole target
//!   row to the DML operator, which writes the unassigned columns back as
//!   they are (or deletes by key). Carrying is not reading: a masked column
//!   only counts as read where an expression references it.
//! - **Reads of the target row.** Where predicates, join conditions or
//!   assignments do reference a masked column of the target row, the
//!   target's `Get` is masked as for a query, and lowering joins the input
//!   on the primary key to a second, unmasked read of the table, which
//!   carries the row written. So assignments see masked values, and the
//!   other columns are written back unmasked.
//! - **`RETURNING`.** A masked column of the written row is masked in
//!   `RETURNING`: the DML operator returns the whole row, and a projection
//!   above it computes the original `RETURNING` items over it, masking
//!   per row as a later read would.
//! - **Copies from governed reads** (`INSERT ... SELECT`, assignments from
//!   another table) read through those reads' masks like any query.
//!
//! The lowered plan is resolved again, which lowers the policies'
//! subqueries to applies below each barrier, and verified as trusted.

use std::collections::HashMap;

use super::error::SecurityError;
use super::eval::EntityStore;
use super::expr::{Action, EntityUid, Mask, SecExpr};
use super::lower::Lowerer;
use super::provider::{ColumnAccess, PartialRequest, PolicyProvider, Residual, SecurityContext};
use crate::catalog::Catalog;
use crate::expr::{Expr, NamedExpr};
use crate::names::{BindingId, ColumnId, ColumnRef, FunctionRef, TableId, TableRef, TargetColumn};
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
    /// The columns to mask, by the binding of the `Get` reading them or
    /// of the DML target row `RETURNING` reads.
    masks: Masks,
    /// The largest binding in use, for allocating fresh ones.
    last_binding: u32,
}

/// Masked columns by the binding of the `Get` that reads them: each
/// column with the condition under which its value shows, and its mask.
type Masks = HashMap<BindingId, Vec<(ColumnId, SecExpr, Mask)>>;

/// The name of the function that chooses between a value and its mask.
pub const POLICY_MASK: &str = "policy_mask";

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

/// Find the binding of the `Get` of `table` that carries an update's or
/// delete's target row: the one whose columns `input` outputs.
fn target_source(input: &Rel, table: TableId, catalog: &dyn Catalog) -> Option<BindingId> {
    /// Collect the bindings of every `Get` of `table`.
    fn gets(rel: &Rel, table: TableId, out: &mut Vec<BindingId>) {
        if let Rel::Get {
            table: TableRef::Id(t),
            binding: Some(b),
            ..
        } = rel
            && *t == table
        {
            out.push(*b);
        }
        rel.inputs().into_iter().for_each(|i| gets(i, table, out));
    }
    let mut found = Vec::new();
    gets(input, table, &mut found);
    let outputs = crate::optimize::output_columns(input, catalog);
    found
        .into_iter()
        .find(|b| outputs.iter().any(|(ob, _)| ob == b))
}

/// Return the `RETURNING` items and binding of a DML operator.
fn returning_of(rel: &mut Rel) -> Option<(&mut Vec<NamedExpr>, &mut Option<BindingId>)> {
    match rel {
        Rel::Insert {
            returning, binding, ..
        }
        | Rel::Update {
            returning, binding, ..
        }
        | Rel::Delete {
            returning, binding, ..
        } => Some((returning, binding)),
        _ => None,
    }
}

impl Lowering<'_> {
    /// Allocate a binding the plan does not use yet.
    fn fresh_binding(&mut self) -> BindingId {
        self.last_binding += 1;
        BindingId::new(self.last_binding)
    }

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

    /// Lower `condition`, a residual over the resource row given by `resource`.
    fn lower_condition(
        &mut self,
        table: TableId,
        condition: &SecExpr,
        resource: &dyn Fn(ColumnId) -> Expr,
    ) -> Result<Expr, SecurityError> {
        let entity = self.catalog.entity_for_table(table).ok_or_else(|| {
            SecurityError::Invalid(format!("table {} is not governed", table.get()))
        })?;
        let table_def = self
            .catalog
            .table_by_id(table)
            .ok_or_else(|| SecurityError::Undeclared(format!("table {}", table.get())))?;
        let mut lowerer = Lowerer {
            catalog: self.catalog,
            entity,
            table: table_def,
            resource,
            principal_and_ancestors: self.principal_and_ancestors.clone(),
            next_alias: self.next_alias,
        };
        let lowered = lowerer.condition(condition)?;
        self.next_alias = lowerer.next_alias;
        Ok(lowered)
    }

    /// Wrap `input`, which reads `table` bound to `binding`, in a projection
    /// that masks the columns in `masks` and passes the rest through.
    fn mask(
        &mut self,
        input: Rel,
        table: TableId,
        binding: BindingId,
        masks: &[(ColumnId, SecExpr, Mask)],
    ) -> Result<Rel, SecurityError> {
        let def = self
            .catalog
            .table_by_id(table)
            .ok_or_else(|| SecurityError::Undeclared(format!("table {}", table.get())))?;
        let resource = |c: ColumnId| Expr::Column(ColumnRef::bound(binding, c));
        let mut exprs = Vec::new();
        for column in &def.columns {
            let value = resource(column.id);
            let Some((_, visible, mask)) = masks.iter().find(|(c, ..)| *c == column.id) else {
                exprs.push(NamedExpr::from(value));
                continue;
            };
            exprs.push(NamedExpr {
                expr: self.masked_value(table, column.id, visible, mask, &resource)?,
                name: Some(column.name.clone()),
            });
        }
        Ok(Rel::Project {
            input: Box::new(input),
            exprs,
            binding: Some(binding),
        })
    }

    /// Construct `policy_mask(visible, value, mask(value))` for `column` of
    /// `table`, over the resource row given by `resource`.
    fn masked_value(
        &mut self,
        table: TableId,
        column: ColumnId,
        visible: &SecExpr,
        mask: &Mask,
        resource: &dyn Fn(ColumnId) -> Expr,
    ) -> Result<Expr, SecurityError> {
        let def = self
            .catalog
            .table_by_id(table)
            .ok_or_else(|| SecurityError::Undeclared(format!("table {}", table.get())))?;
        let column = def
            .column_by_id(column)
            .ok_or_else(|| SecurityError::Undeclared(format!("column {}", column.get())))?;
        if !matches!(column.ty.data, crate::types::DataType::Scalar(_)) {
            return Err(SecurityError::Unsupported(format!(
                "masking column {} of type {}",
                column.name, column.ty.data
            )));
        }
        let value = resource(column.id);
        let visible = self.lower_condition(table, visible, resource)?;
        let masked = match mask {
            // A mask has the column's type, so a query's result types don't
            // depend on who runs it (ADR-0034).
            Mask::Null if !column.ty.optional => {
                return Err(SecurityError::Unsupported(format!(
                    "a NULL mask on column {}, whose type isn't optional; mask it with a \
                     function of its type, or deny it",
                    column.name
                )));
            }
            Mask::Null => Expr::Cast {
                expr: Box::new(Expr::Literal(Value::Null)),
                to: column.ty.data.clone(),
            },
            Mask::Function(f) => Expr::Call {
                func: FunctionRef::Named(f.as_str().into()),
                args: vec![value.clone()],
            },
        };
        Ok(Expr::Call {
            func: FunctionRef::Named(POLICY_MASK.into()),
            args: vec![visible, value, masked],
        })
    }

    /// Join `input`, an update's or delete's input whose target row
    /// (bound to `source`) is masked, on the primary key to an unmasked
    /// read of `table` bound to `raw`, which then carries the target row.
    ///
    /// The masked row's primary key is never masked (see
    /// [`check_columns`]), and both reads see the same snapshot, so each
    /// input row meets exactly its own row.
    fn join_unmasked(
        &mut self,
        input: Rel,
        table: TableId,
        source: BindingId,
        raw: BindingId,
    ) -> Result<Rel, SecurityError> {
        let def = self
            .catalog
            .table_by_id(table)
            .ok_or_else(|| SecurityError::Undeclared(format!("table {}", table.get())))?;
        let key: Vec<ColumnId> = if def.primary_key.is_empty() {
            def.columns.iter().map(|c| c.id).collect()
        } else {
            def.primary_key.clone()
        };
        let on = Expr::And(
            key.iter()
                .map(|&c| Expr::Binary {
                    op: crate::expr::BinaryOp::Eq,
                    left: Box::new(Expr::Column(ColumnRef::bound(source, c))),
                    right: Box::new(Expr::Column(ColumnRef::bound(raw, c))),
                })
                .collect(),
        );
        self.next_alias += 1;
        let alias = crate::names::Ident::new(format!(
            "{}{}",
            super::marker::POLICY_ALIAS_PREFIX,
            self.next_alias
        ));
        Ok(Rel::Join {
            kind: crate::rel::JoinKind::Inner,
            left: Box::new(input),
            right: Box::new(Rel::Get {
                table: TableRef::Id(table),
                alias: Some(alias),
                binding: Some(raw),
            }),
            on,
        })
    }

    /// Mask the masked columns `RETURNING` reads from the row `dml` writes
    /// to `table`, bound to `target`.
    ///
    /// The DML operator returns the whole written row instead, and a
    /// projection above it, bound like the original `RETURNING`, computes
    /// the original items over that row with each masked column replaced
    /// by its masked value, decided for the written row.
    fn mask_returning(
        &mut self,
        mut dml: Rel,
        table: TableId,
        target: Option<BindingId>,
    ) -> Result<Rel, SecurityError> {
        let Some((target, masks)) = target.and_then(|t| Some((t, self.masks.remove(&t)?))) else {
            return Ok(dml);
        };
        let def = self
            .catalog
            .table_by_id(table)
            .ok_or_else(|| SecurityError::Undeclared(format!("table {}", table.get())))?;
        let row = self.fresh_binding();
        let (returning, binding) =
            returning_of(&mut dml).ok_or_else(|| SecurityError::Invalid("not DML".into()))?;
        let items = std::mem::replace(
            returning,
            def.columns
                .iter()
                .map(|c| NamedExpr {
                    expr: Expr::Column(ColumnRef::bound(target, c.id)),
                    name: Some(c.name.clone()),
                })
                .collect(),
        );
        let output = binding.replace(row);
        let resource = |c: ColumnId| Expr::Column(ColumnRef::bound(row, c));
        let mut replacements = HashMap::new();
        for (column, visible, mask) in &masks {
            replacements.insert(
                *column,
                self.masked_value(table, *column, visible, mask, &resource)?,
            );
        }
        let exprs = items
            .into_iter()
            .map(|mut item| {
                crate::optimize::rewrite_expr(&mut item.expr, &|b, c| {
                    (b == target).then(|| replacements.get(&c).cloned().unwrap_or(resource(c)))
                });
                item
            })
            .collect();
        Ok(Rel::Project {
            input: Box::new(dml),
            exprs,
            binding: output,
        })
    }

    /// Lower the input of an update or delete of `table` (`action`), and
    /// return it with the binding that carries the target row.
    ///
    /// Where the statement reads a masked column of the target row, the
    /// target's `Get` is masked like any other, and the input is joined to
    /// an unmasked read of the table that carries the row instead.
    fn target_input(
        &mut self,
        input: Rel,
        table: TableId,
        action: Action,
    ) -> Result<(Rel, BindingId), SecurityError> {
        let masked = target_source(&input, table, self.catalog)
            .filter(|source| self.masks.contains_key(source));
        let input = self.rel(input, Some((table, action)))?;
        if let Some(source) = masked {
            let raw = self.fresh_binding();
            return Ok((self.join_unmasked(input, table, source, raw)?, raw));
        }
        let source = find_get(&input, table).ok_or_else(|| {
            SecurityError::Invalid("update or delete input does not read its target".into())
        })?;
        Ok((input, source))
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
                let filtered = match self.condition(t, &actions, &resource)? {
                    None => get,
                    Some(residual) => Rel::PolicyFilter {
                        input: Box::new(get),
                        residual,
                    },
                };
                match self.masks.remove(&b) {
                    Some(masks) => self.mask(filtered, t, b, &masks)?,
                    None => filtered,
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
                let target_row = target.binding;
                let insert = Rel::Insert {
                    target,
                    columns,
                    input: Box::new(input),
                    returning,
                    binding,
                };
                self.mask_returning(insert, t, target_row)?
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
                let (input, source) = self.target_input(*input, t, Action::Update)?;
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
                let target_row = target.binding;
                let update = Rel::Update {
                    target,
                    input: Box::new(input),
                    assignments,
                    returning,
                    binding,
                };
                self.mask_returning(update, t, target_row)?
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
                let (input, _) = self.target_input(*input, t, Action::Delete)?;
                let target_row = target.binding;
                let delete = Rel::Delete {
                    target,
                    input: Box::new(input),
                    returning,
                    binding,
                };
                self.mask_returning(delete, t, target_row)?
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

/// The rows DML operators write: for each, the target table, the binding
/// of the written row (which only `RETURNING` reads), and for an update or
/// delete, the binding of the `Get` carrying the target row.
struct Written {
    /// Each written row's binding, by table.
    targets: Vec<(TableId, BindingId)>,
    /// The bindings carrying update and delete target rows.
    sources: Vec<BindingId>,
}

/// Collect the rows the DML operators in `rel` write.
fn written(rel: &Rel, catalog: &dyn Catalog, out: &mut Written) {
    if let Rel::Insert { target, .. } | Rel::Update { target, .. } | Rel::Delete { target, .. } =
        rel
        && let TableRef::Id(t) = &target.table
    {
        if let Some(b) = target.binding {
            out.targets.push((*t, b));
        }
        if let Rel::Update { input, .. } | Rel::Delete { input, .. } = rel
            && let Some(source) = target_source(input, *t, catalog)
        {
            out.sources.push(source);
        }
    }
    rel.inputs()
        .into_iter()
        .for_each(|i| written(i, catalog, out));
}

/// Fail if `plan` reads a column of a governed table the principal may not
/// read, and return the columns to mask where they are read.
///
/// A column counts as read if any expression references it, or if the
/// whole row of its relation is consumed by position: the plan's output,
/// a set operation, an insert or `DISTINCT`. Policy residuals are added
/// later and are not checked: the engine may read any column to enforce
/// policies.
///
/// A DML operator's target row is carried, not read: the row an update or
/// delete changes, and the row it writes, count as read only where an
/// expression (a predicate, an assignment, `RETURNING`) references their
/// columns. A masked column of a carried target row consumed by position
/// (`DISTINCT` in an update's input) counts as denied, since the row must
/// reach the operator unmasked; so does a masked primary key column read
/// there, which the unmasked target row is joined on.
fn check_columns(
    plan: &Plan,
    catalog: &dyn Catalog,
    provider: &dyn PolicyProvider,
    context: &SecurityContext,
    store: &dyn EntityStore,
) -> Result<Masks, SecurityError> {
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
    let mut dml = Written {
        targets: Vec::new(),
        sources: Vec::new(),
    };
    written(&plan.root, catalog, &mut dml);
    // The written row is the DML operator's own; only `RETURNING` reads it.
    for (_, target) in &dml.targets {
        whole_rows.remove(target);
    }
    let mut masks = Masks::new();
    for (table, binding) in sources {
        let (Some(entity), Some(def)) =
            (catalog.entity_for_table(table), catalog.table_by_id(table))
        else {
            continue;
        };
        let carried = dml.sources.contains(&binding);
        for attribute in &entity.attributes {
            let whole = whole_rows.contains(&binding);
            let used = whole || read.contains(&(binding, attribute.column));
            if !used {
                continue;
            }
            let denied = || SecurityError::ColumnDenied {
                table: def.name.to_string(),
                column: attribute.name.to_string(),
            };
            match provider.column_mask(context, &entity.name, &attribute.name, store)? {
                ColumnAccess::Allow => {}
                ColumnAccess::Deny => return Err(denied()),
                ColumnAccess::Masked { .. }
                    if carried
                        && (whole
                            || def.primary_key.is_empty()
                            || def.primary_key.contains(&attribute.column)) =>
                {
                    return Err(denied());
                }
                ColumnAccess::Masked { visible, mask } => {
                    masks
                        .entry(binding)
                        .or_default()
                        .push((attribute.column, visible, mask))
                }
            }
        }
    }
    Ok(masks)
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
    let masks = check_columns(plan, catalog, provider, context, store)?;
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
        masks,
        last_binding: crate::optimize::max_binding(&plan.root),
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
