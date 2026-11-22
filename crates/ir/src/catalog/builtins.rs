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
/// error if it is NULL; security lowering uses it for absent attributes).
/// Masking (ADR-0032): `redact(text)` (always `****`), `mask_last4(text)`
/// (every character but the last four replaced by `*`), and
/// `policy_mask(visible, value, masked)` (`value` where `visible` is true,
/// else `masked`), which security lowering uses for masked columns; it is
/// volatile so the optimizer never moves a mask's condition, and `EXPLAIN`
/// hides it. Aggregates: `count` (with no argument
/// for `count(*)`, or one argument of any scalar type), `sum`, `avg`,
/// `min`, `max`, `bool_and` and `bool_or`. Three-valued logic (ADR-0034):
/// `and3`, `or3` and `not3`, which are leakproof since they can't fail.
/// No other built-in function is leakproof yet: each needs an audit first
/// (ADR-0011).
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
    let text = || s(S::Text).option();

    add("lower", scalar, vec![s(S::Text)], text(), pure(true, false));
    add("upper", scalar, vec![s(S::Text)], text(), pure(true, false));
    add(
        "length",
        scalar,
        vec![s(S::Text)],
        s(S::Int4).option(),
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
        add("abs", scalar, vec![s(t)], s(t).option(), pure(true, false));
    }
    let stable = FunctionProperties {
        volatility: Volatility::Stable,
        deterministic: false,
        ..pure(false, false)
    };
    add("now", scalar, vec![], s(S::Timestamptz).plain(), stable);
    let volatile = FunctionProperties {
        volatility: Volatility::Volatile,
        deterministic: false,
        ..pure(false, false)
    };
    add(
        "random",
        scalar,
        vec![],
        s(S::Float8).plain(),
        volatile.clone(),
    );
    add(
        "gen_random_uuid",
        scalar,
        vec![],
        s(S::Uuid).plain(),
        volatile,
    );

    let count = s(S::Int8).plain();
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
            s(S::Int8).plain(),
            pure(true, false),
        );
    }
    add(
        "sum",
        aggregate,
        vec![s(S::Float8)],
        s(S::Float8).plain(),
        pure(true, false),
    );
    for t in [S::Int2, S::Int4, S::Int8, S::Float8] {
        add(
            "avg",
            aggregate,
            vec![s(t)],
            s(S::Float8).option(),
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
                s(t).option(),
                pure(true, false),
            );
        }
    }
    for name in ["bool_and", "bool_or"] {
        add(
            name,
            aggregate,
            vec![s(S::Bool)],
            s(S::Bool).option(),
            pure(true, false),
        );
    }
    // Added in I4; later built-ins go after it so earlier IDs never change.
    for t in ScalarType::ALL {
        add(
            "assert_present",
            scalar,
            vec![s(t)],
            s(t).plain(),
            pure(false, false),
        );
    }
    // Added in F2 (ADR-0032).
    add(
        "redact",
        scalar,
        vec![s(S::Text)],
        text(),
        pure(true, false),
    );
    add(
        "mask_last4",
        scalar,
        vec![s(S::Text)],
        text(),
        pure(true, false),
    );
    let mask = FunctionProperties {
        volatility: Volatility::Volatile,
        deterministic: true,
        ..pure(false, false)
    };
    for t in ScalarType::ALL {
        add(
            "policy_mask",
            scalar,
            vec![s(S::Bool), s(t), s(t)],
            s(t).option(),
            mask.clone(),
        );
    }
    // Added with ADR-0034 phase 2: SQL's three-valued logic on optional booleans.
    let bool3 = || s(S::Bool).option();
    add(
        "and3",
        scalar,
        vec![s(S::Bool), s(S::Bool)],
        bool3(),
        pure(false, true),
    );
    add(
        "or3",
        scalar,
        vec![s(S::Bool), s(S::Bool)],
        bool3(),
        pure(false, true),
    );
    add("not3", scalar, vec![s(S::Bool)], bool3(), pure(false, true));
    out
}

/// Return a one-sentence description of the built-in function `name`, for
/// documentation; `None` if no built-in has that name.
///
/// Every name [`builtin_functions`] returns has a description; the book's
/// function reference is generated from these and the definitions.
///
/// # Examples
///
/// ```
/// use interlaken_ir::catalog::builtin_description;
///
/// assert!(builtin_description("lower").is_some());
/// assert!(builtin_description("no_such_function").is_none());
/// ```
#[must_use]
pub fn builtin_description(name: &str) -> Option<&'static str> {
    Some(match name {
        "lower" => "The text with every character converted to lower case.",
        "upper" => "The text with every character converted to upper case.",
        "length" => "The number of characters (not bytes) in the text.",
        "substr" => {
            "`substr(t, start, count)`: the characters of `t` from 1-based position `start` for \
             `count` characters; positions before the first character count towards `count`, \
             and a negative `count` is an error (`22011`)."
        }
        "abs" => "The absolute value; the minimum integer of its type overflows (`22003`).",
        "now" => "The time the current transaction started; the same for every call in it.",
        "random" => {
            "A pseudo-random value in [0, 1), from a generator seeded when the transaction starts."
        }
        "gen_random_uuid" => "A random version 4 UUID, from the same generator as `random`.",
        "count" => {
            "With no argument (`count(*)`), the number of rows; with one, the number of rows \
             whose argument is not NULL."
        }
        "sum" => {
            "The sum of the non-NULL values, or 0 if there are none, unlike SQL's NULL \
             (ADR-0034). Integer sums are exact and return `int8`, failing with `22003` if the \
             total does not fit."
        }
        "avg" => "The mean of the non-NULL values as `float8`, or NULL if there are none.",
        "min" => "The smallest non-NULL value, or NULL if there are none.",
        "max" => "The largest non-NULL value, or NULL if there are none.",
        "bool_and" => "True if every non-NULL value is true; NULL if there are none.",
        "bool_or" => "True if any non-NULL value is true; NULL if there are none.",
        "assert_present" => {
            "Its argument, or an error (`22004`) if it is NULL. Security lowering uses it for \
             entity attributes a policy reads that may be absent."
        }
        "redact" => "A column mask: always `****`.",
        "mask_last4" => {
            "A column mask: every character but the last four replaced by `*`; text of four \
             characters or fewer is unchanged."
        }
        "policy_mask" => {
            "`policy_mask(visible, value, masked)`: `value` where `visible` is true, else \
             `masked`. Security lowering uses it for \
             masked columns; it is volatile so the optimizer never moves the condition, and \
             `EXPLAIN` hides it."
        }
        "and3" => {
            "SQL's three-valued AND of optional booleans: false if either is false, else NULL \
             if either is NULL, else true. The resolver uses it for `and` over optional values."
        }
        "or3" => {
            "SQL's three-valued OR of optional booleans: true if either is true, else NULL if \
             either is NULL, else false. The resolver uses it for `or` over optional values."
        }
        "not3" => {
            "SQL's three-valued NOT of an optional boolean: NULL for NULL. The resolver uses it \
             for `not` of an optional value."
        }
        _ => return None,
    })
}

/// Return whether a call of `def` with arguments that are optional where
/// `args_optional` says may be absent.
///
/// A function's result is optional if its declared result type is, or if
/// it is strict and an argument is optional. `policy_mask(visible, value,
/// masked)` is optional only if `value` or `masked` is, so a masked column
/// keeps its type whoever runs the query (ADR-0034).
///
/// # Examples
///
/// ```
/// use interlaken_ir::catalog::{builtin_functions, call_optional};
///
/// let functions = builtin_functions();
/// let mask = functions.iter().find(|f| f.name.to_string() == "policy_mask").unwrap();
/// // A plain column masked with a plain value stays plain.
/// assert!(!call_optional(mask, &[false, false, false]));
/// // Masking it with NULL would make it optional.
/// assert!(call_optional(mask, &[false, false, true]));
/// ```
#[must_use]
pub fn call_optional(def: &FunctionDef, args_optional: &[bool]) -> bool {
    if def.name.to_string() == "policy_mask" && args_optional.len() == 3 {
        return args_optional[1] || args_optional[2];
    }
    def.returns.optional || (def.properties.strict && args_optional.iter().any(|o| *o))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert every built-in function name has a description.
    fn every_builtin_is_described() {
        for f in builtin_functions() {
            let name = f.name.to_string();
            assert!(
                builtin_description(&name).is_some(),
                "{name} has no description"
            );
        }
    }
}
