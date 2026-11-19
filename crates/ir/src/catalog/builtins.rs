//! Built-in functions and the properties of built-in operators (ADR-0011).

use super::defs::{FunctionDef, FunctionKind, FunctionProperties, Volatility};
use super::memory::{CatalogError, InMemoryCatalog};
use crate::expr::{BinaryOp, UnaryOp};
use crate::names::FunctionId;
use crate::types::{DataType, ScalarType, Type};
use crate::typing::{implicit, is_comparable, is_integer};

/// Function IDs below this are reserved for built-ins.
pub const FIRST_USER_FUNCTION_ID: u32 = 1000;

/// Properties of an immutable, deterministic function.
fn pure(strict: bool, leakproof: bool) -> FunctionProperties {
    FunctionProperties {
        volatility: Volatility::Immutable,
        strict,
        leakproof,
        deterministic: true,
        cost: 1.0,
        selectivity: None,
    }
}

/// Return a scalar data type.
fn s(scalar: ScalarType) -> DataType {
    DataType::Scalar(scalar)
}

/// Return every built-in function overload, with IDs from 1 in a fixed order.
///
/// Scalar functions: `lower`, `upper`, `length`, `substr`, `abs`, `now`,
/// `random`, `gen_random_uuid`, and `assert_present` (its argument, or an
/// error if it is NULL; security lowering uses it for absent attributes). Aggregates: `count` (with no argument
/// for `count(*)`, or one argument of any scalar type), `sum`, `avg`,
/// `min`, `max`, `bool_and` and `bool_or`. No built-in function is
/// leakproof yet: each needs an audit first (ADR-0011).
///
/// # Examples
///
/// ```
/// use interlaken_ir::catalog::builtin_functions;
///
/// let functions = builtin_functions();
/// assert!(functions.iter().any(|f| f.name.to_string() == "count" && f.args.is_empty()));
/// ```
#[must_use]
pub fn builtin_functions() -> Vec<FunctionDef> {
    use ScalarType as S;
    let mut out = Vec::new();
    let mut add = |name: &str, kind, args: Vec<DataType>, returns: Type, properties| {
        let id = u32::try_from(out.len() + 1).expect("built-in IDs fit in u32");
        out.push(FunctionDef {
            id: FunctionId::new(id),
            name: name.into(),
            kind,
            args,
            returns,
            properties,
        });
    };
    let scalar = FunctionKind::Scalar;
    let aggregate = FunctionKind::Aggregate;
    let text = || s(S::Text).nullable();

    add("lower", scalar, vec![s(S::Text)], text(), pure(true, false));
    add("upper", scalar, vec![s(S::Text)], text(), pure(true, false));
    add(
        "length",
        scalar,
        vec![s(S::Text)],
        s(S::Int4).nullable(),
        pure(true, false),
    );
    add(
        "substr",
        scalar,
        vec![s(S::Text), s(S::Int4), s(S::Int4)],
        text(),
        pure(true, false),
    );
    for t in [S::Int2, S::Int4, S::Int8, S::Float8] {
        add(
            "abs",
            scalar,
            vec![s(t)],
            s(t).nullable(),
            pure(true, false),
        );
    }
    let stable = FunctionProperties {
        volatility: Volatility::Stable,
        deterministic: false,
        ..pure(false, false)
    };
    add("now", scalar, vec![], s(S::Timestamptz).not_null(), stable);
    let volatile = FunctionProperties {
        volatility: Volatility::Volatile,
        deterministic: false,
        ..pure(false, false)
    };
    add(
        "random",
        scalar,
        vec![],
        s(S::Float8).not_null(),
        volatile.clone(),
    );
    add(
        "gen_random_uuid",
        scalar,
        vec![],
        s(S::Uuid).not_null(),
        volatile,
    );

    let count = s(S::Int8).not_null();
    add(
        "count",
        aggregate,
        vec![],
        count.clone(),
        pure(false, false),
    );
    for t in ScalarType::ALL {
        add(
            "count",
            aggregate,
            vec![s(t)],
            count.clone(),
            pure(true, false),
        );
    }
    for t in [S::Int2, S::Int4, S::Int8] {
        add(
            "sum",
            aggregate,
            vec![s(t)],
            s(S::Int8).nullable(),
            pure(true, false),
        );
    }
    add(
        "sum",
        aggregate,
        vec![s(S::Float8)],
        s(S::Float8).nullable(),
        pure(true, false),
    );
    for t in [S::Int2, S::Int4, S::Int8, S::Float8] {
        add(
            "avg",
            aggregate,
            vec![s(t)],
            s(S::Float8).nullable(),
            pure(true, false),
        );
    }
    for name in ["min", "max"] {
        for t in ScalarType::ALL
            .into_iter()
            .filter(|t| is_comparable(&s(*t)))
        {
            add(
                name,
                aggregate,
                vec![s(t)],
                s(t).nullable(),
                pure(true, false),
            );
        }
    }
    for name in ["bool_and", "bool_or"] {
        add(
            name,
            aggregate,
            vec![s(S::Bool)],
            s(S::Bool).nullable(),
            pure(true, false),
        );
    }
    // Added in I4; later built-ins go after it so earlier IDs never change.
    for t in ScalarType::ALL {
        add(
            "assert_present",
            scalar,
            vec![s(t)],
            s(t).not_null(),
            pure(false, false),
        );
    }
    out
}

/// Add every built-in function to `catalog`.
///
/// # Errors
///
/// Returns a [`CatalogError`] if one of the built-in IDs or overloads is already taken.
pub fn install_builtins(catalog: &mut InMemoryCatalog) -> Result<(), CatalogError> {
    builtin_functions()
        .into_iter()
        .try_for_each(|f| catalog.add_function(f))
}

/// Return the properties of a built-in unary operator.
///
/// `NOT`, `IS NULL` and `IS NOT NULL` are leakproof. Negation is not: it
/// overflows on the minimum integer and on infinite floats.
#[must_use]
pub fn unary_op_properties(op: UnaryOp) -> FunctionProperties {
    match op {
        UnaryOp::Not => pure(true, true),
        UnaryOp::IsNull | UnaryOp::IsNotNull => pure(false, true),
        UnaryOp::Neg => pure(true, false),
    }
}

/// Return the properties of a built-in binary operator on operands of the given type.
///
/// The operand type is not consulted yet; it is part of the signature so
/// per-type audits (for example, text comparison under collations) can
/// refine the answer without changing callers.
///
/// Comparisons and `IS [NOT] DISTINCT FROM` cannot raise errors and are
/// leakproof. Arithmetic can overflow or divide by zero, `LIKE` rejects
/// malformed patterns, and concatenation is conservatively treated like
/// arithmetic, so none of them is.
#[must_use]
pub fn binary_op_properties(op: BinaryOp, _operand: &DataType) -> FunctionProperties {
    match op {
        _ if op.is_comparison() => pure(true, true),
        BinaryOp::IsDistinctFrom | BinaryOp::IsNotDistinctFrom => pure(false, true),
        _ => pure(true, false),
    }
}

/// Return the properties of an explicit cast from `from` to `to`.
///
/// Identity casts and lossless widenings between integer types and from
/// `date` to `timestamptz` are leakproof; every other cast can fail on some value and is not.
#[must_use]
pub fn cast_properties(from: &DataType, to: &DataType) -> FunctionProperties {
    let widening = implicit(from, to) && !(is_integer(from) && to == &s(ScalarType::Float8));
    pure(true, widening)
}
