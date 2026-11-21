//! Implicit conversions: making operand types match.

use super::error::ResolveError;
use crate::expr::Expr;
use crate::text::print_expr;
use crate::types::{DataType, ScalarType, Type};
use crate::typing;
use crate::value::Value;

/// A resolved expression and its type; `None` for an untyped NULL.
#[derive(Clone, Debug)]
pub(crate) struct Typed {
    /// The resolved expression.
    pub(crate) expr: Expr,
    /// Its type, unless it is an untyped NULL.
    pub(crate) ty: Option<Type>,
}

/// Which conversions a context allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Lossless conversions only, for operands.
    Implicit,
    /// Also numeric narrowing and conversion to text, for stored values.
    Assignment,
}

/// Return the smallest integer type holding a bare integer literal.
fn literal_width(v: i64) -> ScalarType {
    if i16::try_from(v).is_ok() {
        ScalarType::Int2
    } else if i32::try_from(v).is_ok() {
        ScalarType::Int4
    } else {
        ScalarType::Int8
    }
}

/// Return the value of a bare integer literal, which adapts to its context.
fn flexible(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(Value::Int8(v)) => Some(*v),
        _ => None,
    }
}

/// Construct an explicit cast of NULL to `to`.
pub(crate) fn typed_null(to: DataType) -> Typed {
    Typed {
        expr: Expr::Cast {
            expr: Box::new(Expr::Literal(Value::Null)),
            to: to.clone(),
        },
        ty: Some(to.option()),
    }
}

/// Convert `typed` to `to`, rewriting flexible literals and inserting casts as `mode` allows.
pub(crate) fn coerce(typed: Typed, to: &DataType, mode: Mode) -> Result<Typed, ResolveError> {
    let Some(ty) = &typed.ty else {
        return Ok(typed_null(to.clone()));
    };
    if &ty.data == to {
        return Ok(typed);
    }
    if let Some(v) = flexible(&typed.expr) {
        let literal = match typing::scalar(to) {
            Some(ScalarType::Int2) => i16::try_from(v).ok().map(Value::Int2),
            Some(ScalarType::Int4) => i32::try_from(v).ok().map(Value::Int4),
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            Some(ScalarType::Float8) if (v as f64) as i64 == v && v.unsigned_abs() < (1 << 53) => {
                Some(Value::Float8(v as f64))
            }
            _ => None,
        };
        if let Some(value) = literal {
            return Ok(Typed {
                expr: Expr::Literal(value),
                ty: Some(to.clone().plain()),
            });
        }
    }
    let allowed = match mode {
        Mode::Implicit => typing::implicit(&ty.data, to),
        Mode::Assignment => typing::assignment(&ty.data, to),
    };
    if !allowed {
        return Err(ResolveError::Type(format!(
            "cannot convert {} of type {} to {to}",
            print_expr(&typed.expr),
            ty.data
        )));
    }
    let nullable = ty.optional;
    Ok(Typed {
        expr: Expr::Cast {
            expr: Box::new(typed.expr),
            to: to.clone(),
        },
        ty: Some(to.clone().option().with_optional(nullable)),
    })
}

/// Convert `items` to their common type (`default` if all are untyped NULLs).
///
/// Bare integer literals count as the smallest integer type holding them
/// when other operands have types, so they adapt rather than widen the
/// other operands. Returns the converted expressions and the common type,
/// optional if any item is.
pub(crate) fn unify(
    items: Vec<Typed>,
    default: ScalarType,
) -> Result<(Vec<Expr>, Type), ResolveError> {
    let typed: Vec<&Typed> = items.iter().filter(|t| t.ty.is_some()).collect();
    let has_fixed = typed.iter().any(|t| flexible(&t.expr).is_none());
    let candidates: Vec<DataType> = typed
        .iter()
        .map(|t| match flexible(&t.expr) {
            Some(v) if has_fixed => DataType::Scalar(literal_width(v)),
            _ => t.ty.as_ref().expect("filtered to typed").data.clone(),
        })
        .collect();
    let target = if candidates.is_empty() {
        DataType::Scalar(default)
    } else {
        typing::common_type(&candidates).ok_or_else(|| {
            let shown: Vec<String> = candidates.iter().map(ToString::to_string).collect();
            ResolveError::Type(format!("no common type for {}", shown.join(", ")))
        })?
    };
    let nullable = items
        .iter()
        .any(|t| t.ty.as_ref().is_none_or(|ty| ty.optional));
    let exprs = items
        .into_iter()
        .map(|t| coerce(t, &target, Mode::Implicit).map(|t| t.expr))
        .collect::<Result<_, _>>()?;
    Ok((exprs, target.option().with_optional(nullable)))
}

/// Return `typed` as a boolean predicate (casting an untyped NULL), naming `what` on failure.
pub(crate) fn predicate(typed: Typed, what: &str) -> Result<Typed, ResolveError> {
    let boolean = DataType::Scalar(ScalarType::Bool);
    match &typed.ty {
        None => Ok(typed_null(boolean)),
        Some(ty) if ty.data == boolean => Ok(typed),
        Some(ty) => Err(ResolveError::Type(format!(
            "{what} must be bool, not {} ({})",
            ty.data,
            print_expr(&typed.expr)
        ))),
    }
}

/// Return the type of `typed`, giving an untyped NULL the type `default`.
pub(crate) fn settle(typed: Typed, default: ScalarType) -> (Expr, Type) {
    match typed.ty {
        Some(ty) => (typed.expr, ty),
        None => {
            let t = typed_null(DataType::Scalar(default));
            (t.expr, t.ty.expect("typed_null has a type"))
        }
    }
}
