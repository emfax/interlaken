//! Shared helpers for rewrite rules: bindings, references, purity and types.

use std::collections::{HashMap, HashSet};

use crate::catalog::{
    Catalog, Volatility, binary_op_properties, cast_properties, unary_op_properties,
};
use crate::expr::Expr;
use crate::names::{BindingId, ColumnId, ColumnRef, FunctionRef};
use crate::rel::Rel;
use crate::types::{DataType, Type};
use crate::typing;
use crate::value::Value;

/// Column types by `(binding, column)`.
pub(crate) type Types = HashMap<(BindingId, ColumnId), Type>;

/// Return every binding introduced in `rel` or below.
pub(crate) fn produced(rel: &Rel) -> HashSet<BindingId> {
    let mut out: HashSet<BindingId> = rel.bindings().into_iter().collect();
    for input in rel.inputs() {
        out.extend(produced(input));
    }
    out
}

/// Add the bindings `expr` references to `out`.
pub(crate) fn expr_bindings(expr: &Expr, out: &mut HashSet<BindingId>) {
    if let Expr::Column(ColumnRef::Bound { binding, .. }) = expr {
        out.insert(*binding);
    }
    for child in expr.children() {
        expr_bindings(child, out);
    }
}

/// Return the bindings `expr` references.
pub(crate) fn bindings_of(expr: &Expr) -> HashSet<BindingId> {
    let mut out = HashSet::new();
    expr_bindings(expr, &mut out);
    out
}

/// Return the bindings referenced in `rel` but introduced outside it (its correlation).
pub(crate) fn free(rel: &Rel) -> HashSet<BindingId> {
    let mut referenced = HashSet::new();
    /// Collect every referenced binding in the subtree.
    fn walk(rel: &Rel, out: &mut HashSet<BindingId>) {
        for e in rel.exprs() {
            expr_bindings(e, out);
        }
        for input in rel.inputs() {
            walk(input, out);
        }
    }
    walk(rel, &mut referenced);
    let inside = produced(rel);
    referenced.retain(|b| !inside.contains(b));
    referenced
}

/// Rewrite every column reference in `expr` for which `f` returns a replacement.
pub(crate) fn rewrite_expr(expr: &mut Expr, f: &dyn Fn(BindingId, ColumnId) -> Option<Expr>) {
    if let Expr::Column(ColumnRef::Bound { binding, column }) = expr
        && let Some(replacement) = f(*binding, *column)
    {
        *expr = replacement;
        return;
    }
    for child in expr.children_mut() {
        rewrite_expr(child, f);
    }
}

/// Rewrite column references throughout `rel` and below.
pub(crate) fn rewrite_rel(rel: &mut Rel, f: &dyn Fn(BindingId, ColumnId) -> Option<Expr>) {
    for e in rel.exprs_mut() {
        rewrite_expr(e, f);
    }
    for input in rel.inputs_mut() {
        rewrite_rel(input, f);
    }
}

/// Return a reference to `(binding, column)`.
pub(crate) fn column(binding: BindingId, column: ColumnId) -> Expr {
    Expr::Column(ColumnRef::bound(binding, column))
}

/// Return the column ID for position `i`.
pub(crate) fn col_id(i: usize) -> ColumnId {
    ColumnId::new(u32::try_from(i).expect("column positions fit in u32"))
}

/// Split `expr` into its conjuncts, flattening nested `AND`s.
pub(crate) fn conjuncts(expr: Expr) -> Vec<Expr> {
    match expr {
        Expr::And(items) => items.into_iter().flat_map(conjuncts).collect(),
        other => vec![other],
    }
}

/// Combine conjuncts into one predicate (`true` when there are none).
pub(crate) fn conjoin(mut items: Vec<Expr>) -> Expr {
    match items.len() {
        0 => Expr::Literal(Value::Bool(true)),
        1 => items.pop().expect("one item"),
        _ => Expr::And(items),
    }
}

/// Return the largest binding used in `rel`, so new ones can be allocated above it.
pub(crate) fn max_binding(rel: &Rel) -> u32 {
    let mut max = rel
        .bindings()
        .into_iter()
        .map(BindingId::get)
        .max()
        .unwrap_or(0);
    for e in rel.exprs() {
        for b in bindings_of(e) {
            max = max.max(b.get());
        }
    }
    for input in rel.inputs() {
        max = max.max(max_binding(input));
    }
    max
}

/// Hands out bindings not yet used in the plan.
pub(crate) struct Bindings(pub(crate) u32);

impl Bindings {
    /// Allocate a fresh binding.
    pub(crate) fn fresh(&mut self) -> BindingId {
        self.0 += 1;
        BindingId::new(self.0)
    }
}

/// Return the data type of `expr` where it can be derived from `types` and the catalog.
pub(crate) fn data_type(expr: &Expr, types: &Types, catalog: &dyn Catalog) -> Option<DataType> {
    Some(match expr {
        Expr::Column(ColumnRef::Bound { binding, column }) => {
            types.get(&(*binding, *column))?.data.clone()
        }
        Expr::Literal(v) => typing::value_type(v)?.data,
        Expr::Param { ty, .. } => ty.data.clone(),
        Expr::Cast { to, .. } => to.clone(),
        Expr::Call {
            func: FunctionRef::Id(id),
            ..
        } => catalog.function_by_id(*id)?.returns.data.clone(),
        Expr::Unary { op, expr } => {
            typing::unary(*op, &data_type(expr, types, catalog)?.option())
                .ok()?
                .data
        }
        Expr::Binary { op, left, right } => {
            let l = data_type(left, types, catalog)?.option();
            let r = data_type(right, types, catalog)?.option();
            typing::binary(*op, &l, &r).ok()?.data
        }
        Expr::And(_) | Expr::Or(_) => DataType::Scalar(crate::types::ScalarType::Bool),
        Expr::Coalesce(items) => data_type(items.first()?, types, catalog)?,
        Expr::NullIf { left, .. } => data_type(left, types, catalog)?,
        Expr::Case { branches, .. } => data_type(&branches.first()?.then, types, catalog)?,
        Expr::Array(items) => DataType::Array(Box::new(
            data_type(items.first()?, types, catalog)?.option(),
        )),
        Expr::Try { expr, .. } => data_type(expr, types, catalog)?,
        _ => return None,
    })
}

/// Return `true` if evaluating `expr` cannot reveal its inputs through
/// errors or side effects, so it may run below a security barrier (ADR-0018).
///
/// Conservative: anything whose properties cannot be established is not leakproof.
pub(crate) fn leakproof(expr: &Expr, types: &Types, catalog: &dyn Catalog) -> bool {
    if let Expr::Try {
        expr: inner,
        default,
    } = expr
    {
        // Errors inside are contained; only side effects could leak.
        return !volatile(inner, catalog) && leakproof(default, types, catalog);
    }
    let this = match expr {
        Expr::Column(_) | Expr::Literal(_) | Expr::Param { .. } => true,
        Expr::And(_) | Expr::Or(_) | Expr::Case { .. } | Expr::Coalesce(_) | Expr::Array(_) => true,
        Expr::Call {
            func: FunctionRef::Id(id),
            ..
        } => catalog
            .function_by_id(*id)
            .is_some_and(|f| f.properties.leakproof),
        Expr::Unary { op, .. } => unary_op_properties(*op).leakproof,
        Expr::Binary { op, left, .. } => data_type(left, types, catalog)
            .is_some_and(|ty| binary_op_properties(*op, &ty).leakproof),
        Expr::NullIf { left, .. } => data_type(left, types, catalog)
            .is_some_and(|ty| binary_op_properties(crate::expr::BinaryOp::Eq, &ty).leakproof),
        Expr::Cast { expr: operand, to } => match operand.as_ref() {
            Expr::Literal(Value::Null) => true,
            Expr::Array(items) if items.is_empty() => true,
            operand => data_type(operand, types, catalog)
                .is_some_and(|from| cast_properties(&from, to).leakproof),
        },
        Expr::Call { .. } | Expr::Subquery { .. } | Expr::Try { .. } => false,
    };
    this && expr
        .children()
        .into_iter()
        .all(|c| leakproof(c, types, catalog))
}

/// Return `true` if `expr` calls a volatile function, so moving it would change how often it runs.
pub(crate) fn volatile(expr: &Expr, catalog: &dyn Catalog) -> bool {
    let this = match expr {
        Expr::Call {
            func: FunctionRef::Id(id),
            ..
        } => catalog
            .function_by_id(*id)
            .is_none_or(|f| f.properties.volatility == Volatility::Volatile),
        Expr::Call { .. } | Expr::Subquery { .. } => true,
        _ => false,
    };
    this || expr.children().into_iter().any(|c| volatile(c, catalog))
}

/// Return `true` if `rel` produces at most one row whatever its input.
pub(crate) fn at_most_one_row(rel: &Rel) -> bool {
    match rel {
        Rel::Aggregate { keys, .. } | Rel::Nest { keys, .. } => keys.is_empty(),
        Rel::Limit {
            limit: Some(Expr::Literal(Value::Int8(n))),
            ..
        } => *n <= 1,
        Rel::Values { rows, .. } => rows.len() <= 1,
        Rel::Filter { input, .. }
        | Rel::Project { input, .. }
        | Rel::Sort { input, .. }
        | Rel::Distinct { input }
        | Rel::Limit { input, .. } => at_most_one_row(input),
        _ => false,
    }
}

/// Move a relation out of `rel`, leaving a cheap placeholder to be overwritten.
pub(crate) fn take(rel: &mut Rel) -> Rel {
    std::mem::replace(
        rel,
        Rel::Values {
            columns: Vec::new(),
            rows: Vec::new(),
            binding: None,
        },
    )
}

/// Move an expression out of `expr`, leaving a cheap placeholder to be overwritten.
pub(crate) fn take_expr(expr: &mut Expr) -> Expr {
    std::mem::replace(expr, Expr::And(Vec::new()))
}

/// Return the output columns of `rel`, in order, without types.
pub(crate) fn output_columns(rel: &Rel, catalog: &dyn Catalog) -> Vec<(BindingId, ColumnId)> {
    let numbered = |binding: &Option<BindingId>, n: usize| -> Vec<(BindingId, ColumnId)> {
        binding.map_or_else(Vec::new, |b| (0..n).map(|i| (b, col_id(i))).collect())
    };
    match rel {
        Rel::Get { table, binding, .. } => match (table, binding) {
            (crate::names::TableRef::Id(id), Some(b)) => catalog
                .table_by_id(*id)
                .map(|t| t.columns.iter().map(|c| (*b, c.id)).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        },
        Rel::Filter { input, .. }
        | Rel::PolicyFilter { input, .. }
        | Rel::WriteCheck { input, .. }
        | Rel::Sort { input, .. }
        | Rel::Limit { input, .. }
        | Rel::Distinct { input } => output_columns(input, catalog),
        Rel::Project { exprs, binding, .. } => numbered(binding, exprs.len()),
        Rel::Aggregate {
            keys,
            aggs,
            binding,
            ..
        } => numbered(binding, keys.len() + aggs.len()),
        Rel::Nest { keys, binding, .. } => numbered(binding, keys.len() + 1),
        Rel::Values {
            columns, binding, ..
        } => numbered(binding, columns.len()),
        Rel::SetOp { left, binding, .. } => numbered(binding, output_columns(left, catalog).len()),
        Rel::Insert {
            returning, binding, ..
        }
        | Rel::Update {
            returning, binding, ..
        }
        | Rel::Delete {
            returning, binding, ..
        } => numbered(binding, returning.len()),
        Rel::Join {
            kind, left, right, ..
        } => {
            let mut out = output_columns(left, catalog);
            if !matches!(
                kind,
                crate::rel::JoinKind::Semi | crate::rel::JoinKind::Anti
            ) {
                out.extend(output_columns(right, catalog));
            }
            out
        }
        Rel::Apply { kind, outer, inner } => {
            let mut out = output_columns(outer, catalog);
            if !matches!(
                kind,
                crate::rel::ApplyKind::Semi | crate::rel::ApplyKind::Anti
            ) {
                out.extend(output_columns(inner, catalog));
            }
            out
        }
        Rel::Unnest {
            input,
            columns,
            binding,
            ..
        } => {
            let mut out = output_columns(input, catalog);
            out.extend(numbered(binding, columns.len()));
            out
        }
    }
}
