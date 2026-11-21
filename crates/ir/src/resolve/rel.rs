//! Resolving relational operators.

use std::collections::HashSet;
use std::sync::Arc;

use super::api::Resolver;
use super::coerce::{Mode, Typed, coerce, predicate, settle, unify};
use super::error::ResolveError;
use super::expr::{ExprCx, Lowered, derived_name};
use super::scope::{Scope, ScopeColumn};
use crate::catalog::{FunctionKind, TableDef};
use crate::expr::{Expr, NamedExpr};
use crate::names::{BindingId, ColumnRef, FunctionRef, Ident, TableRef, TargetColumn};
use crate::rel::{AggCall, ApplyKind, Assignment, DmlTarget, JoinKind, NamedAgg, Rel, SortKey};
use crate::types::{DataType, Field, ScalarType, Type};
use crate::typing;
use crate::value::Value;

/// Return the scopes visible to an operator's expressions: its input first, then the enclosing ones.
fn env<'a>(local: &'a Scope, outer: &[&'a Scope]) -> Vec<&'a Scope> {
    std::iter::once(local)
        .chain(outer.iter().copied())
        .collect()
}

/// Return the columns a join or apply outputs, padding outer sides with NULLs.
fn joined(
    left: Scope,
    right: Scope,
    left_only: bool,
    nullable_left: bool,
    nullable_right: bool,
) -> Scope {
    let pad = |mut c: ScopeColumn, nullable: bool| {
        c.ty.optional |= nullable;
        c
    };
    let mut columns: Vec<ScopeColumn> = left
        .columns
        .into_iter()
        .map(|c| pad(c, nullable_left))
        .collect();
    if !left_only {
        columns.extend(right.columns.into_iter().map(|c| pad(c, nullable_right)));
    }
    Scope { columns }
}

/// Return `true` if `expr` contains a subquery anywhere.
fn has_subquery(expr: &Expr) -> bool {
    matches!(expr, Expr::Subquery { .. }) || expr.children().into_iter().any(has_subquery)
}

/// Collect the conjuncts of `expr`, flattening nested `AND`s.
fn flatten_and<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
    match expr {
        Expr::And(items) => items.iter().for_each(|item| flatten_and(item, out)),
        other => out.push(other),
    }
}

/// Wrap `input` in the applies of lowered subqueries; their columns are hidden.
fn wrap(mut input: Rel, mut scope: Scope, lowered: Vec<Lowered>) -> (Rel, Scope) {
    for Lowered {
        kind,
        inner,
        scope: inner_scope,
    } in lowered
    {
        input = Rel::Apply {
            kind,
            outer: Box::new(input),
            inner: Box::new(inner),
        };
        if matches!(kind, ApplyKind::Inner | ApplyKind::Left | ApplyKind::Single) {
            let nullable = kind != ApplyKind::Inner;
            scope
                .columns
                .extend(inner_scope.columns.into_iter().map(|mut c| {
                    c.hidden = true;
                    c.ty.optional |= nullable;
                    c
                }));
        }
    }
    (input, scope)
}

impl Resolver<'_> {
    /// Look up a table by name or ID.
    fn table(&self, table: &TableRef) -> Result<Arc<TableDef>, ResolveError> {
        match table {
            TableRef::Named(name) => self.catalog.table_by_name(name),
            TableRef::Id(id) => self.catalog.table_by_id(*id),
        }
        .ok_or_else(|| {
            ResolveError::UnknownTable(match table {
                TableRef::Named(name) => name.to_string(),
                TableRef::Id(id) => format!("(tid {})", id.get()),
            })
        })
    }

    /// Return the scope of `table`'s columns under a fresh binding, qualified by `alias` or the table name.
    fn table_scope(
        &mut self,
        table: &TableDef,
        alias: Option<&Ident>,
        original: Option<BindingId>,
    ) -> Result<(BindingId, Scope), ResolveError> {
        let binding = self.fresh()?;
        let qualifier = alias.cloned().unwrap_or_else(|| table.name.last().clone());
        let columns = table
            .columns
            .iter()
            .map(|c| ScopeColumn {
                binding,
                column: c.id,
                original,
                qualifier: Some(qualifier.clone()),
                name: Some(c.name.clone()),
                ty: c.ty.clone(),
                hidden: false,
                source: Some(table.id),
            })
            .collect();
        Ok((binding, Scope { columns }))
    }

    /// Resolve output items, naming them by `as` or by the column they reference.
    fn items(
        &mut self,
        items: &[NamedExpr],
        cx: &mut ExprCx<'_>,
    ) -> Result<Vec<(NamedExpr, Type)>, ResolveError> {
        items
            .iter()
            .map(|item| {
                let (expr, ty) = settle(self.expr(&item.expr, cx)?, ScalarType::Text);
                let name = item
                    .name
                    .clone()
                    .or_else(|| derived_name(&item.expr, &expr, &cx.env));
                Ok((NamedExpr { expr, name }, ty))
            })
            .collect()
    }

    /// Resolve an aggregate call.
    fn agg(
        &mut self,
        item: &NamedAgg,
        cx: &mut ExprCx<'_>,
    ) -> Result<(NamedAgg, Type), ResolveError> {
        let agg = &item.agg;
        let args = agg
            .args
            .iter()
            .map(|a| self.expr(a, cx))
            .collect::<Result<Vec<_>, _>>()?;
        let (def, args) = self.pick_function(&agg.func, args, FunctionKind::Aggregate)?;
        let filter = agg
            .filter
            .as_ref()
            .map(|f| {
                self.expr(f, cx)
                    .and_then(|t| predicate(t, "an aggregate filter"))
                    .map(|t| t.expr)
            })
            .transpose()?;
        let name = item.name.clone().or_else(|| Some(def.name.last().clone()));
        Ok((
            NamedAgg {
                agg: AggCall {
                    func: FunctionRef::Id(def.id),
                    args,
                    distinct: agg.distinct,
                    filter,
                },
                name,
            },
            def.returns.clone(),
        ))
    }

    /// Resolve the target of a DML operator.
    fn target(
        &mut self,
        target: &DmlTarget,
    ) -> Result<(Arc<TableDef>, DmlTarget, Scope), ResolveError> {
        let table = self.table(&target.table)?;
        let (binding, scope) = self.table_scope(&table, target.alias.as_ref(), target.binding)?;
        Ok((
            table.clone(),
            DmlTarget {
                table: TableRef::Id(table.id),
                alias: target.alias.clone(),
                binding: Some(binding),
            },
            scope,
        ))
    }

    /// Look up a target column of `table`.
    fn target_column<'t>(
        table: &'t TableDef,
        column: &TargetColumn,
    ) -> Result<&'t crate::catalog::ColumnDef, ResolveError> {
        match column {
            TargetColumn::Named(name) => table.column_by_name(name.as_str()),
            TargetColumn::Id(id) => table.column_by_id(*id),
        }
        .ok_or_else(|| ResolveError::UnknownColumn(format!("{column:?} of {}", table.name)))
    }

    /// Resolve `RETURNING` items over the target row and return them with the output scope.
    fn returning(
        &mut self,
        returning: &[NamedExpr],
        target_row: &Scope,
        original: Option<BindingId>,
    ) -> Result<(Vec<NamedExpr>, Option<BindingId>, Scope), ResolveError> {
        let mut cx = ExprCx::plain(vec![target_row], "RETURNING");
        let items = self.items(returning, &mut cx)?;
        let binding = self.fresh()?;
        let scope = Scope::derived(
            binding,
            original,
            items.iter().map(|(n, t)| (n.name.clone(), t.clone())),
        )?;
        Ok((
            items.into_iter().map(|(n, _)| n).collect(),
            Some(binding),
            scope,
        ))
    }

    /// Fail unless `input` reads exactly one instance of `table`: the rows a DML operator changes.
    fn expect_target_rows(input: &Scope, table: &TableDef) -> Result<(), ResolveError> {
        let bindings: HashSet<BindingId> = input
            .columns
            .iter()
            .filter(|c| c.source == Some(table.id))
            .map(|c| c.binding)
            .collect();
        match bindings.len() {
            1 => Ok(()),
            0 => Err(ResolveError::Invalid(format!(
                "the input of an update or delete must read the target table {}",
                table.name
            ))),
            _ => Err(ResolveError::Invalid(format!(
                "the input of an update or delete reads the target table {} more than once",
                table.name
            ))),
        }
    }

    /// Wrap `rel` in a projection casting its columns to `types` where they differ.
    fn cast_columns(
        &mut self,
        rel: Rel,
        scope: Scope,
        types: &[DataType],
        mode: Mode,
    ) -> Result<(Rel, Scope), ResolveError> {
        let visible: Vec<ScopeColumn> = scope.visible().cloned().collect();
        if visible.iter().zip(types).all(|(c, t)| &c.ty.data == t) && !scope.has_hidden() {
            return Ok((rel, scope));
        }
        let mut exprs = Vec::new();
        let mut columns = Vec::new();
        for (c, t) in visible.iter().zip(types) {
            let typed = coerce(
                Typed {
                    expr: Expr::Column(ColumnRef::bound(c.binding, c.column)),
                    ty: Some(c.ty.clone()),
                },
                t,
                mode,
            )?;
            exprs.push(NamedExpr {
                expr: typed.expr,
                name: c.name.clone(),
            });
            columns.push((c.name.clone(), typed.ty.expect("columns are typed")));
        }
        let binding = self.fresh()?;
        Ok((
            Rel::Project {
                input: Box::new(rel),
                exprs,
                binding: Some(binding),
            },
            Scope::derived(binding, None, columns)?,
        ))
    }

    /// Resolve a relation; `outer` holds the scopes of enclosing queries.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn rel(
        &mut self,
        rel: &Rel,
        outer: &[&Scope],
    ) -> Result<(Rel, Scope), ResolveError> {
        Ok(match rel {
            Rel::Get {
                table,
                alias,
                binding,
            } => {
                let def = self.table(table)?;
                let (new, scope) = self.table_scope(&def, alias.as_ref(), *binding)?;
                (
                    Rel::Get {
                        table: TableRef::Id(def.id),
                        alias: alias.clone(),
                        binding: Some(new),
                    },
                    scope,
                )
            }
            Rel::Filter {
                input,
                predicate: pred,
            } => {
                let (input, scope) = self.rel(input, outer)?;
                let mut conjuncts = Vec::new();
                flatten_and(pred, &mut conjuncts);
                let mut cx = ExprCx::lowering(env(&scope, outer));
                let mut remaining = Vec::new();
                for conjunct in conjuncts {
                    if !self.lower_conjunct(conjunct, &mut cx)? {
                        let typed = self.expr(conjunct, &mut cx)?;
                        remaining.push(predicate(typed, "a filter predicate")?.expr);
                    }
                }
                let lowered = std::mem::take(&mut cx.lowered);
                drop(cx);
                let (input, scope) = wrap(input, scope, lowered);
                let predicate = match remaining.len() {
                    0 => return Ok((input, scope)),
                    1 => remaining.pop().expect("one conjunct"),
                    _ => Expr::And(remaining),
                };
                (
                    Rel::Filter {
                        input: Box::new(input),
                        predicate,
                    },
                    scope,
                )
            }
            Rel::PolicyFilter { input, residual }
            | Rel::WriteCheck {
                input,
                check: residual,
            } => {
                let (input, scope) = self.rel(input, outer)?;
                let mut cx = ExprCx::lowering(env(&scope, outer));
                let typed = self.expr(residual, &mut cx)?;
                let residual = predicate(typed, "a policy residual")?.expr;
                let lowered = std::mem::take(&mut cx.lowered);
                drop(cx);
                let (input, scope) = wrap(input, scope, lowered);
                let input = Box::new(input);
                let rel = if matches!(rel, Rel::PolicyFilter { .. }) {
                    Rel::PolicyFilter { input, residual }
                } else {
                    Rel::WriteCheck {
                        input,
                        check: residual,
                    }
                };
                (rel, scope)
            }
            Rel::Project {
                input,
                exprs,
                binding,
            } => {
                let (input, scope) = self.rel(input, outer)?;
                let mut cx = ExprCx::lowering(env(&scope, outer));
                let items = self.items(exprs, &mut cx)?;
                let lowered = std::mem::take(&mut cx.lowered);
                drop(cx);
                let (input, _) = wrap(input, scope, lowered);
                let new = self.fresh()?;
                let out = Scope::derived(
                    new,
                    *binding,
                    items.iter().map(|(n, t)| (n.name.clone(), t.clone())),
                )?;
                (
                    Rel::Project {
                        input: Box::new(input),
                        exprs: items.into_iter().map(|(n, _)| n).collect(),
                        binding: Some(new),
                    },
                    out,
                )
            }
            Rel::Join {
                kind,
                left,
                right,
                on,
            } => {
                if has_subquery(on) {
                    if *kind != JoinKind::Inner {
                        return Err(ResolveError::NotSupported(
                            "subqueries in outer, semi or anti join conditions".into(),
                        ));
                    }
                    // An inner join's condition is a filter over the cross join.
                    let rewritten = Rel::Filter {
                        input: Box::new(Rel::Join {
                            kind: JoinKind::Inner,
                            left: left.clone(),
                            right: right.clone(),
                            on: Expr::Literal(Value::Bool(true)),
                        }),
                        predicate: on.clone(),
                    };
                    return self.rel(&rewritten, outer);
                }
                let (left, left_scope) = self.rel(left, outer)?;
                let (right, right_scope) = self.rel(right, outer)?;
                let both = Scope {
                    columns: left_scope
                        .columns
                        .iter()
                        .chain(&right_scope.columns)
                        .cloned()
                        .collect(),
                };
                let mut cx = ExprCx::plain(env(&both, outer), "join conditions");
                let typed = self.expr(on, &mut cx)?;
                let on = predicate(typed, "a join condition")?.expr;
                let scope = match kind {
                    JoinKind::Inner => joined(left_scope, right_scope, false, false, false),
                    JoinKind::Left => joined(left_scope, right_scope, false, false, true),
                    JoinKind::Right => joined(left_scope, right_scope, false, true, false),
                    JoinKind::Full => joined(left_scope, right_scope, false, true, true),
                    JoinKind::Semi | JoinKind::Anti => left_scope,
                };
                (
                    Rel::Join {
                        kind: *kind,
                        left: Box::new(left),
                        right: Box::new(right),
                        on,
                    },
                    scope,
                )
            }
            Rel::Apply {
                kind,
                outer: outer_rel,
                inner,
            } => {
                let (outer_rel, outer_scope) = self.rel(outer_rel, outer)?;
                let (inner, inner_scope) = self.rel(inner, &env(&outer_scope, outer))?;
                let scope = match kind {
                    ApplyKind::Inner => joined(outer_scope, inner_scope, false, false, false),
                    ApplyKind::Left | ApplyKind::Single => {
                        joined(outer_scope, inner_scope, false, false, true)
                    }
                    ApplyKind::Semi | ApplyKind::Anti => outer_scope,
                };
                (
                    Rel::Apply {
                        kind: *kind,
                        outer: Box::new(outer_rel),
                        inner: Box::new(inner),
                    },
                    scope,
                )
            }
            Rel::Aggregate {
                input,
                keys,
                aggs,
                binding,
            } => {
                let (input, scope) = self.rel(input, outer)?;
                let mut cx = ExprCx::lowering(env(&scope, outer));
                let keys = self.items(keys, &mut cx)?;
                for (key, ty) in &keys {
                    if !typing::is_comparable(&ty.data) {
                        return Err(ResolveError::Type(format!(
                            "cannot group by {} of type {}",
                            crate::text::print_expr(&key.expr),
                            ty.data
                        )));
                    }
                }
                let aggs = aggs
                    .iter()
                    .map(|a| self.agg(a, &mut cx))
                    .collect::<Result<Vec<_>, _>>()?;
                let lowered = std::mem::take(&mut cx.lowered);
                drop(cx);
                let (input, _) = wrap(input, scope, lowered);
                let new = self.fresh()?;
                let columns = keys
                    .iter()
                    .map(|(n, t)| (n.name.clone(), t.clone()))
                    .chain(aggs.iter().map(|(a, t)| (a.name.clone(), t.clone())));
                let out = Scope::derived(new, *binding, columns)?;
                (
                    Rel::Aggregate {
                        input: Box::new(input),
                        keys: keys.into_iter().map(|(n, _)| n).collect(),
                        aggs: aggs.into_iter().map(|(a, _)| a).collect(),
                        binding: Some(new),
                    },
                    out,
                )
            }
            Rel::Distinct { input } => {
                let (input, scope) = self.rel(input, outer)?;
                if let Some(c) = scope
                    .columns
                    .iter()
                    .find(|c| !typing::is_comparable(&c.ty.data))
                {
                    return Err(ResolveError::Type(format!(
                        "DISTINCT cannot compare column {} of type {}",
                        c.name.as_ref().map_or("?", Ident::as_str),
                        c.ty.data
                    )));
                }
                (
                    Rel::Distinct {
                        input: Box::new(input),
                    },
                    scope,
                )
            }
            Rel::Sort { input, keys } => {
                let (input, scope) = self.rel(input, outer)?;
                let mut cx = ExprCx::lowering(env(&scope, outer));
                let mut resolved = Vec::new();
                for key in keys {
                    let (expr, ty) = settle(self.expr(&key.expr, &mut cx)?, ScalarType::Text);
                    if !typing::is_comparable(&ty.data) {
                        return Err(ResolveError::Type(format!("cannot sort by {}", ty.data)));
                    }
                    resolved.push(SortKey {
                        expr,
                        descending: key.descending,
                        nulls_first: key.nulls_first,
                    });
                }
                let lowered = std::mem::take(&mut cx.lowered);
                drop(cx);
                let (input, scope) = wrap(input, scope, lowered);
                (
                    Rel::Sort {
                        input: Box::new(input),
                        keys: resolved,
                    },
                    scope,
                )
            }
            Rel::Limit {
                input,
                limit,
                offset,
            } => {
                let (input, scope) = self.rel(input, outer)?;
                let int8 = DataType::Scalar(ScalarType::Int8);
                let mut count = |e: &Option<Expr>| -> Result<Option<Expr>, ResolveError> {
                    e.as_ref()
                        .map(|e| {
                            let mut cx = ExprCx::plain(Vec::new(), "LIMIT and OFFSET");
                            let typed = self.expr(e, &mut cx)?;
                            Ok(coerce(typed, &int8, Mode::Implicit)?.expr)
                        })
                        .transpose()
                };
                let limit = count(limit)?;
                let offset = count(offset)?;
                (
                    Rel::Limit {
                        input: Box::new(input),
                        limit,
                        offset,
                    },
                    scope,
                )
            }
            Rel::SetOp {
                op,
                all,
                left,
                right,
                binding,
            } => {
                let (left, left_scope) = self.rel(left, outer)?;
                let (left, left_scope) = self.visible_only(left, left_scope)?;
                let (right, right_scope) = self.rel(right, outer)?;
                let (right, right_scope) = self.visible_only(right, right_scope)?;
                if left_scope.columns.len() != right_scope.columns.len() {
                    return Err(ResolveError::Invalid(format!(
                        "set operation inputs have {} and {} columns",
                        left_scope.columns.len(),
                        right_scope.columns.len()
                    )));
                }
                let mut types = Vec::new();
                for (l, r) in left_scope.columns.iter().zip(&right_scope.columns) {
                    let common =
                        typing::common_type([&l.ty.data, &r.ty.data]).ok_or_else(|| {
                            ResolveError::Type(format!(
                                "set operation columns have types {} and {}",
                                l.ty.data, r.ty.data
                            ))
                        })?;
                    if !(*all && *op == crate::rel::SetOpKind::Union)
                        && !typing::is_comparable(&common)
                    {
                        return Err(ResolveError::Type(format!(
                            "set operation cannot compare {common}"
                        )));
                    }
                    types.push(common);
                }
                let names: Vec<Option<Ident>> =
                    left_scope.columns.iter().map(|c| c.name.clone()).collect();
                let nullable: Vec<bool> = left_scope
                    .columns
                    .iter()
                    .zip(&right_scope.columns)
                    .map(|(l, r)| l.ty.optional || r.ty.optional)
                    .collect();
                let (left, _) = self.cast_columns(left, left_scope, &types, Mode::Implicit)?;
                let (right, _) = self.cast_columns(right, right_scope, &types, Mode::Implicit)?;
                let new = self.fresh()?;
                let columns = names.into_iter().zip(
                    types
                        .into_iter()
                        .zip(nullable)
                        .map(|(t, n)| t.option().with_optional(n)),
                );
                let out = Scope::derived(new, *binding, columns)?;
                (
                    Rel::SetOp {
                        op: *op,
                        all: *all,
                        left: Box::new(left),
                        right: Box::new(right),
                        binding: Some(new),
                    },
                    out,
                )
            }
            Rel::Values {
                columns,
                rows,
                binding,
            } => {
                if rows.is_empty() {
                    return Err(ResolveError::Invalid(
                        "values needs at least one row".into(),
                    ));
                }
                let mut cells: Vec<Vec<Typed>> = vec![Vec::new(); columns.len()];
                let mut cx = ExprCx::plain(outer.to_vec(), "VALUES");
                for row in rows {
                    if row.len() != columns.len() {
                        return Err(ResolveError::Invalid(format!(
                            "a values row has {} expressions for {} columns",
                            row.len(),
                            columns.len()
                        )));
                    }
                    for (cell, e) in cells.iter_mut().zip(row) {
                        cell.push(self.expr(e, &mut cx)?);
                    }
                }
                let mut resolved_columns = Vec::new();
                let mut types = Vec::new();
                for cell in cells {
                    let (exprs, ty) = unify(cell, ScalarType::Text)?;
                    resolved_columns.push(exprs);
                    types.push(ty);
                }
                let resolved_rows = (0..rows.len())
                    .map(|i| resolved_columns.iter().map(|c| c[i].clone()).collect())
                    .collect();
                let new = self.fresh()?;
                let out =
                    Scope::derived(new, *binding, columns.iter().cloned().map(Some).zip(types))?;
                (
                    Rel::Values {
                        columns: columns.clone(),
                        rows: resolved_rows,
                        binding: Some(new),
                    },
                    out,
                )
            }
            Rel::Nest {
                input,
                keys,
                fields,
                name,
                binding,
            } => {
                let (input, scope) = self.rel(input, outer)?;
                let mut cx = ExprCx::lowering(env(&scope, outer));
                let keys = self.items(keys, &mut cx)?;
                for (_, ty) in &keys {
                    if !typing::is_comparable(&ty.data) {
                        return Err(ResolveError::Type(format!("cannot nest by {}", ty.data)));
                    }
                }
                let fields = self.items(fields, &mut cx)?;
                let lowered = std::mem::take(&mut cx.lowered);
                drop(cx);
                let (input, _) = wrap(input, scope, lowered);
                let record = fields
                    .iter()
                    .map(|(f, ty)| {
                        f.name
                            .clone()
                            .map(|n| Field::new(n, ty.clone()))
                            .ok_or_else(|| {
                                ResolveError::Invalid("every nest field needs a name".into())
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let array = DataType::Array(Box::new(DataType::Record(record).plain())).plain();
                let new = self.fresh()?;
                let columns = keys
                    .iter()
                    .map(|(n, t)| (n.name.clone(), t.clone()))
                    .chain(std::iter::once((Some(name.clone()), array)));
                let out = Scope::derived(new, *binding, columns)?;
                (
                    Rel::Nest {
                        input: Box::new(input),
                        keys: keys.into_iter().map(|(n, _)| n).collect(),
                        fields: fields.into_iter().map(|(n, _)| n).collect(),
                        name: name.clone(),
                        binding: Some(new),
                    },
                    out,
                )
            }
            Rel::Unnest {
                input,
                expr,
                columns,
                binding,
            } => {
                let (input, scope) = self.rel(input, outer)?;
                let mut cx = ExprCx::lowering(env(&scope, outer));
                let typed = self.expr(expr, &mut cx)?;
                let lowered = std::mem::take(&mut cx.lowered);
                drop(cx);
                let (input, mut scope) = wrap(input, scope, lowered);
                let ty = typed
                    .ty
                    .ok_or_else(|| ResolveError::Type("cannot unnest NULL".into()))?;
                let DataType::Array(element) = &ty.data else {
                    return Err(ResolveError::Type(format!(
                        "unnest needs an array, not {}",
                        ty.data
                    )));
                };
                let element_columns: Vec<(Ident, Type)> = match &element.data {
                    DataType::Record(fields) => fields
                        .iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                f.ty.clone()
                                    .with_optional(f.ty.optional || element.optional),
                            )
                        })
                        .collect(),
                    _ => vec![(Ident::new("unnest"), (**element).clone())],
                };
                let names: Vec<Ident> = if columns.is_empty() {
                    element_columns.iter().map(|(n, _)| n.clone()).collect()
                } else if columns.len() == element_columns.len() {
                    columns.clone()
                } else {
                    return Err(ResolveError::Invalid(format!(
                        "unnest names {} columns but each element has {}",
                        columns.len(),
                        element_columns.len()
                    )));
                };
                let new = self.fresh()?;
                let element_scope = Scope::derived(
                    new,
                    *binding,
                    names
                        .iter()
                        .cloned()
                        .map(Some)
                        .zip(element_columns.into_iter().map(|(_, t)| t)),
                )?;
                scope.columns.extend(element_scope.columns);
                (
                    Rel::Unnest {
                        input: Box::new(input),
                        expr: typed.expr,
                        columns: names,
                        binding: Some(new),
                    },
                    scope,
                )
            }
            Rel::Insert {
                target,
                columns,
                input,
                returning,
                binding,
            } => {
                let (input, input_scope) = self.rel(input, outer)?;
                let (input, input_scope) = self.visible_only(input, input_scope)?;
                // The target binding comes after any casts over the input, so
                // resolving the result again numbers bindings identically.
                let table = self.table(&target.table)?;
                let defs: Vec<&crate::catalog::ColumnDef> = if columns.is_empty() {
                    table.columns.iter().collect()
                } else {
                    columns
                        .iter()
                        .map(|c| Self::target_column(&table, c))
                        .collect::<Result<_, _>>()?
                };
                let mut seen = HashSet::new();
                for def in &defs {
                    if !seen.insert(def.id) {
                        return Err(ResolveError::Invalid(format!(
                            "insert names column {} twice",
                            def.name
                        )));
                    }
                }
                if let Some(missing) = table
                    .columns
                    .iter()
                    .find(|c| !c.ty.optional && !seen.contains(&c.id))
                {
                    return Err(ResolveError::Invalid(format!(
                        "insert omits column {}, whose type isn't optional",
                        missing.name
                    )));
                }
                if defs.len() != input_scope.columns.len() {
                    return Err(ResolveError::Invalid(format!(
                        "insert names {} columns but its input has {}",
                        defs.len(),
                        input_scope.columns.len()
                    )));
                }
                let types: Vec<DataType> = defs.iter().map(|d| d.ty.data.clone()).collect();
                let (input, _) = self.cast_columns(input, input_scope, &types, Mode::Assignment)?;
                let (_, target, target_row) = self.target(target)?;
                let column_ids: Vec<TargetColumn> =
                    defs.iter().map(|d| TargetColumn::Id(d.id)).collect();
                let (returning, new, out) = self.returning(returning, &target_row, *binding)?;
                (
                    Rel::Insert {
                        target,
                        columns: column_ids,
                        input: Box::new(input),
                        returning,
                        binding: new,
                    },
                    out,
                )
            }
            Rel::Update {
                target,
                input,
                assignments,
                returning,
                binding,
            } => {
                let (input, scope) = self.rel(input, outer)?;
                let (table, target, target_row) = self.target(target)?;
                Self::expect_target_rows(&scope, &table)?;
                let mut cx = ExprCx::lowering(env(&scope, outer));
                let mut seen = HashSet::new();
                let mut resolved = Vec::new();
                for assignment in assignments {
                    let def = Self::target_column(&table, &assignment.column)?;
                    if !seen.insert(def.id) {
                        return Err(ResolveError::Invalid(format!(
                            "update assigns column {} twice",
                            def.name
                        )));
                    }
                    let typed = self.expr(&assignment.value, &mut cx)?;
                    if typed.ty.is_none() && !def.ty.optional {
                        return Err(ResolveError::Invalid(format!(
                            "cannot assign NULL to column {}, whose type isn't optional",
                            def.name
                        )));
                    }
                    let value = coerce(typed, &def.ty.data, Mode::Assignment)?.expr;
                    resolved.push(Assignment {
                        column: TargetColumn::Id(def.id),
                        value,
                    });
                }
                let lowered = std::mem::take(&mut cx.lowered);
                drop(cx);
                let (input, _) = wrap(input, scope, lowered);
                let (returning, new, out) = self.returning(returning, &target_row, *binding)?;
                (
                    Rel::Update {
                        target,
                        input: Box::new(input),
                        assignments: resolved,
                        returning,
                        binding: new,
                    },
                    out,
                )
            }
            Rel::Delete {
                target,
                input,
                returning,
                binding,
            } => {
                let (input, scope) = self.rel(input, outer)?;
                let (table, target, target_row) = self.target(target)?;
                Self::expect_target_rows(&scope, &table)?;
                let (returning, new, out) = self.returning(returning, &target_row, *binding)?;
                (
                    Rel::Delete {
                        target,
                        input: Box::new(input),
                        returning,
                        binding: new,
                    },
                    out,
                )
            }
        })
    }
}
