//! Strict typing rules shared by the resolver and the verifier (ADR-0010).
//!
//! These rules accept only exactly matching operand types: resolved IR
//! contains explicit casts and nothing else. The resolver makes operands
//! match (inserting casts allowed by [`implicit`] or [`assignment`]) and
//! then applies the same rules the verifier checks, so the two cannot
//! disagree about what is well typed.

use crate::expr::{BinaryOp, UnaryOp};
use crate::types::{DataType, ScalarType, Type};
use crate::value::Value;

/// Return the scalar type inside `ty`, if it is a scalar.
pub(crate) fn scalar(ty: &DataType) -> Option<ScalarType> {
    match ty {
        DataType::Scalar(s) => Some(*s),
        _ => None,
    }
}

/// Return `true` for `int2`, `int4`, `int8` and `float8`.
pub(crate) fn is_numeric(ty: &DataType) -> bool {
    matches!(
        scalar(ty),
        Some(ScalarType::Int2 | ScalarType::Int4 | ScalarType::Int8 | ScalarType::Float8)
    )
}

/// Return `true` for `int2`, `int4` and `int8`.
pub(crate) fn is_integer(ty: &DataType) -> bool {
    matches!(
        scalar(ty),
        Some(ScalarType::Int2 | ScalarType::Int4 | ScalarType::Int8)
    )
}

/// Return `true` if values of `ty` can be compared and sorted.
///
/// Every scalar except `jsonb`; arrays, records and entities are not
/// comparable in the MVP.
pub(crate) fn is_comparable(ty: &DataType) -> bool {
    scalar(ty).is_some_and(|s| s != ScalarType::Jsonb)
}

/// Return the NOT NULL type of a literal, or `None` for an untyped NULL.
pub(crate) fn value_type(value: &Value) -> Option<Type> {
    value.scalar_type().map(|s| DataType::Scalar(s).not_null())
}

/// Return the `bool` type with the given nullability.
pub(crate) fn boolean(nullable: bool) -> Type {
    DataType::Scalar(ScalarType::Bool)
        .nullable()
        .with_nullable(nullable)
}

/// Return the result type of a unary operator, or why it does not apply.
pub(crate) fn unary(op: UnaryOp, operand: &Type) -> Result<Type, String> {
    match op {
        UnaryOp::Not if operand.data == DataType::Scalar(ScalarType::Bool) => Ok(operand.clone()),
        UnaryOp::Not => Err(format!("NOT needs bool, not {}", operand.data)),
        UnaryOp::Neg if is_numeric(&operand.data) => Ok(operand.clone()),
        UnaryOp::Neg => Err(format!("cannot negate {}", operand.data)),
        UnaryOp::IsNull | UnaryOp::IsNotNull => Ok(boolean(false)),
    }
}

/// Return the result type of a binary operator, or why it does not apply.
pub(crate) fn binary(op: BinaryOp, left: &Type, right: &Type) -> Result<Type, String> {
    let nullable = left.nullable || right.nullable;
    let (l, r) = (&left.data, &right.data);
    let same = l == r;
    let mismatch = || {
        format!(
            "operator {} does not apply to {l} and {r}",
            crate::text::binary_op_name(op)
        )
    };
    let data = match op {
        _ if op.is_comparison() => {
            if same && is_comparable(l) {
                return Ok(boolean(nullable));
            }
            return Err(mismatch());
        }
        BinaryOp::IsDistinctFrom | BinaryOp::IsNotDistinctFrom => {
            if same && is_comparable(l) {
                return Ok(boolean(false));
            }
            return Err(mismatch());
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div if same && is_numeric(l) => {
            l.clone()
        }
        BinaryOp::Mod if same && is_integer(l) => l.clone(),
        BinaryOp::Add | BinaryOp::Sub
            if *l == DataType::Scalar(ScalarType::Date)
                && *r == DataType::Scalar(ScalarType::Int4) =>
        {
            l.clone()
        }
        BinaryOp::Sub if same && *l == DataType::Scalar(ScalarType::Date) => {
            DataType::Scalar(ScalarType::Int4)
        }
        BinaryOp::Concat
            if same && matches!(scalar(l), Some(ScalarType::Text | ScalarType::Bytea)) =>
        {
            l.clone()
        }
        BinaryOp::Like if same && *l == DataType::Scalar(ScalarType::Text) => {
            return Ok(boolean(nullable));
        }
        _ => return Err(mismatch()),
    };
    Ok(Type { data, nullable })
}

/// Return `true` if an explicit cast from `from` to `to` is allowed.
///
/// Identity casts always are. Between scalars: any numeric to any
/// numeric, integers to and from `bool`, anything to and from `text`, and
/// `date` to and from `timestamptz`. Non-scalar types cast only to themselves.
pub(crate) fn explicit(from: &DataType, to: &DataType) -> bool {
    if from == to {
        return true;
    }
    let (Some(f), Some(t)) = (scalar(from), scalar(to)) else {
        return false;
    };
    use ScalarType as S;
    (is_numeric(from) && is_numeric(to))
        || (is_integer(from) && t == S::Bool)
        || (f == S::Bool && is_integer(to))
        || f == S::Text
        || t == S::Text
        || matches!(
            (f, t),
            (S::Date, S::Timestamptz) | (S::Timestamptz, S::Date)
        )
}

/// Return `true` if `from` converts to `to` implicitly, without loss.
///
/// The integer widenings, any integer to `float8`, and `date` to `timestamptz`.
pub(crate) fn implicit(from: &DataType, to: &DataType) -> bool {
    use ScalarType as S;
    from == to
        || matches!(
            (scalar(from), scalar(to)),
            (Some(S::Int2), Some(S::Int4 | S::Int8 | S::Float8))
                | (Some(S::Int4), Some(S::Int8 | S::Float8))
                | (Some(S::Int8), Some(S::Float8))
                | (Some(S::Date), Some(S::Timestamptz))
        )
}

/// Return `true` if a value of `from` may be stored in a column of `to`.
///
/// Implicit conversions plus numeric narrowing (checked for overflow at run
/// time), `timestamptz` to `date`, and anything to `text`, as in Postgres's
/// assignment casts.
pub(crate) fn assignment(from: &DataType, to: &DataType) -> bool {
    use ScalarType as S;
    implicit(from, to)
        || (is_numeric(from) && is_numeric(to))
        || matches!(
            (scalar(from), scalar(to)),
            (Some(S::Timestamptz), Some(S::Date))
        )
        || (scalar(from).is_some() && scalar(to) == Some(S::Text))
}

/// Return the type every type in `types` converts to implicitly, if one of them is such a type.
pub(crate) fn common_type<'a>(
    types: impl IntoIterator<Item = &'a DataType> + Clone,
) -> Option<DataType> {
    types
        .clone()
        .into_iter()
        .find(|candidate| types.clone().into_iter().all(|t| implicit(t, candidate)))
        .cloned()
}

/// Return the comparison that is TRUE exactly when `op` is FALSE.
///
/// # Panics
///
/// Panics if `op` is not a comparison.
pub(crate) fn negate_comparison(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Eq => BinaryOp::Ne,
        BinaryOp::Ne => BinaryOp::Eq,
        BinaryOp::Lt => BinaryOp::Ge,
        BinaryOp::Ge => BinaryOp::Lt,
        BinaryOp::Gt => BinaryOp::Le,
        BinaryOp::Le => BinaryOp::Gt,
        other => panic!("{other:?} is not a comparison"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Return a NOT NULL scalar type.
    fn t(s: ScalarType) -> Type {
        DataType::Scalar(s).not_null()
    }

    #[test]
    /// Assert operators accept exactly matching operands and compute nullability.
    fn operator_types() {
        use ScalarType as S;
        assert_eq!(
            binary(BinaryOp::Add, &t(S::Int4), &t(S::Int4)),
            Ok(t(S::Int4))
        );
        assert!(binary(BinaryOp::Add, &t(S::Int4), &t(S::Int8)).is_err());
        assert!(binary(BinaryOp::Mod, &t(S::Float8), &t(S::Float8)).is_err());
        assert_eq!(
            binary(BinaryOp::Sub, &t(S::Date), &t(S::Date)),
            Ok(t(S::Int4))
        );
        assert_eq!(
            binary(BinaryOp::Add, &t(S::Date), &t(S::Int4)),
            Ok(t(S::Date))
        );
        let nullable = t(S::Text).with_nullable(true);
        assert_eq!(
            binary(BinaryOp::Eq, &nullable, &t(S::Text)),
            Ok(boolean(true))
        );
        assert_eq!(
            binary(BinaryOp::IsDistinctFrom, &nullable, &t(S::Text)),
            Ok(boolean(false))
        );
        assert!(binary(BinaryOp::Eq, &t(S::Jsonb), &t(S::Jsonb)).is_err());
        assert_eq!(unary(UnaryOp::IsNull, &nullable), Ok(boolean(false)));
        assert!(unary(UnaryOp::Not, &t(S::Int4)).is_err());
    }

    #[test]
    /// Assert the cast contexts nest: implicit within assignment within explicit.
    fn cast_contexts_nest() {
        let all: Vec<DataType> = ScalarType::ALL
            .iter()
            .map(|s| DataType::Scalar(*s))
            .collect();
        for from in &all {
            for to in &all {
                if implicit(from, to) {
                    assert!(assignment(from, to), "{from} -> {to}");
                }
                if assignment(from, to) {
                    assert!(explicit(from, to), "{from} -> {to}");
                }
            }
        }
        use ScalarType as S;
        let d = |s| DataType::Scalar(s);
        assert_eq!(
            common_type(&[d(S::Int2), d(S::Int8), d(S::Int4)]),
            Some(d(S::Int8))
        );
        assert_eq!(common_type(&[d(S::Int8), d(S::Text)]), None);
    }
}
