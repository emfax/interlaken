//! Phase 1: simplification (ADR-0014).
//!
//! Constant folding of immutable expressions, boolean normalization, and
//! removal or merging of filters. Boolean simplification never removes an
//! operand that would have been evaluated, because one that raises an
//! error decides the result of an enclosing `TRY` (a policy condition):
//! `x OR true` stays as it is, while `true OR x` becomes `true`.

use super::util::{Types, conjoin, conjuncts, data_type, leakproof, take, take_expr};
use crate::catalog::{Catalog, Volatility};
use crate::expr::{Expr, UnaryOp};
use crate::interp::{Datum, eval_constant};
use crate::names::FunctionRef;
use crate::rel::Rel;
use crate::value::Value;

/// Return `true` if `expr` has no column references or parameters and calls only immutable functions.
fn is_constant(expr: &Expr, catalog: &dyn Catalog) -> bool {
    let this = match expr {
        Expr::Column(_) | Expr::Param { .. } | Expr::Subquery { .. } => false,
        Expr::Call {
            func: FunctionRef::Id(id),
            ..
        } => catalog
            .function_by_id(*id)
            .is_some_and(|f| f.properties.volatility == Volatility::Immutable),
        Expr::Call { .. } => false,
        _ => true,
    };
    this && expr.children().into_iter().all(|c| is_constant(c, catalog))
}

/// Return `true` if `expr` is already in folded form: a literal, a typed NULL or a typed empty array.
fn is_folded(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(_) => true,
        Expr::Cast { expr, .. } => {
            matches!(**expr, Expr::Literal(Value::Null))
                || matches!(&**expr, Expr::Array(items) if items.is_empty())
        }
        _ => false,
    }
}

/// Convert a scalar datum back to a literal.
fn to_literal(d: Datum) -> Option<Value> {
    Some(match d {
        Datum::Bool(v) => Value::Bool(v),
        Datum::Int2(v) => Value::Int2(v),
        Datum::Int4(v) => Value::Int4(v),
        Datum::Int8(v) => Value::Int8(v),
        Datum::Float8(v) => Value::Float8(v),
        Datum::Text(v) => Value::Text(v),
        Datum::Bytea(v) => Value::Bytea(v),
        Datum::Date(v) => Value::Date(v),
        Datum::Timestamptz(v) => Value::Timestamptz(v),
        Datum::Uuid(v) => Value::Uuid(v),
        Datum::Null | Datum::Jsonb(_) | Datum::Array(_) | Datum::Record(_) => return None,
    })
}

/// Fold constant subexpressions of `expr` and normalize its boolean structure.
pub(crate) fn simplify_expr(expr: &mut Expr, types: &Types, catalog: &dyn Catalog) {
    for child in expr.children_mut() {
        simplify_expr(child, types, catalog);
    }
    if !is_folded(expr)
        && !matches!(expr, Expr::Array(_))
        && is_constant(expr, catalog)
        && let Ok(datum) = eval_constant(expr, catalog)
    {
        let folded = if datum.is_null() {
            data_type(expr, types, catalog).map(|to| Expr::Cast {
                expr: Box::new(Expr::Literal(Value::Null)),
                to,
            })
        } else {
            to_literal(datum).map(Expr::Literal)
        };
        if let Some(folded) = folded {
            *expr = folded;
            return;
        }
    }
    let replacement = match expr {
        Expr::And(items) => {
            let mut kept = Vec::new();
            for item in conjuncts(Expr::And(std::mem::take(items))) {
                match item {
                    Expr::Literal(Value::Bool(true)) => {}
                    Expr::Literal(Value::Bool(false)) => {
                        // Operands before it still run first and may raise, which
                        // matters inside TRY; the ones after never run. Earlier
                        // operands can go only if none can raise: a non-raising one
                        // still decides whether a later raising one runs.
                        if kept.iter().all(|e| leakproof(e, types, catalog)) {
                            kept.clear();
                        }
                        kept.push(item);
                        break;
                    }
                    other => kept.push(other),
                }
            }
            Some(conjoin(kept))
        }
        Expr::Or(items) => {
            let mut kept = Vec::new();
            let mut flat = Vec::new();
            for item in std::mem::take(items) {
                match item {
                    Expr::Or(inner) => flat.extend(inner),
                    other => flat.push(other),
                }
            }
            for item in flat {
                match item {
                    Expr::Literal(Value::Bool(false)) => {}
                    Expr::Literal(Value::Bool(true)) => {
                        if kept.iter().all(|e| leakproof(e, types, catalog)) {
                            kept.clear();
                        }
                        kept.push(item);
                        break;
                    }
                    other => kept.push(other),
                }
            }
            Some(match kept.len() {
                0 => Expr::Literal(Value::Bool(false)),
                1 => kept.pop().expect("one item"),
                _ => Expr::Or(kept),
            })
        }
        Expr::Unary {
            op: UnaryOp::Not,
            expr: inner,
        } => match inner.as_mut() {
            Expr::Unary {
                op: UnaryOp::Not,
                expr: x,
            } => Some(take_expr(x)),
            _ => None,
        },
        _ => None,
    };
    if let Some(replacement) = replacement {
        *expr = replacement;
    }
}

/// Simplify every expression in `rel`, drop always-true filters and merge stacked filters.
pub(crate) fn simplify(rel: &mut Rel, types: &Types, catalog: &dyn Catalog) {
    for input in rel.inputs_mut() {
        simplify(input, types, catalog);
    }
    for e in rel.exprs_mut() {
        simplify_expr(e, types, catalog);
    }
    let replacement = match rel {
        Rel::Filter {
            input,
            predicate: Expr::Literal(Value::Bool(true)),
        } => Some(take(input)),
        Rel::Filter { input, predicate } => match input.as_mut() {
            Rel::Filter {
                input: inner,
                predicate: first,
            } => {
                let mut merged = conjuncts(take_expr(first));
                merged.extend(conjuncts(take_expr(predicate)));
                let inner = take(inner);
                Some(Rel::Filter {
                    input: Box::new(inner),
                    predicate: conjoin(merged),
                })
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(replacement) = replacement {
        *rel = replacement;
    }
}
