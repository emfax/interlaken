//! Checking parameter values against the types a plan declares for them.

use std::collections::BTreeMap;
use std::fmt;

use super::datum::Datum;
use crate::expr::Expr;
use crate::rel::Rel;
use crate::types::{DataType, ScalarType, Type};

/// Why parameter values don't fit a plan's declarations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamError {
    /// The plan declares parameters up to `expected`, but `supplied` values were passed.
    Count {
        /// The highest parameter number the plan declares.
        expected: usize,
        /// The number of values passed.
        supplied: usize,
    },
    /// A value isn't of its parameter's declared type.
    Type {
        /// The 1-based parameter number.
        index: u32,
        /// The declared type.
        expected: Type,
        /// The value passed, as `Debug` text.
        value: String,
    },
    /// NULL was passed for a parameter declared NOT NULL.
    Null {
        /// The 1-based parameter number.
        index: u32,
        /// The declared type.
        expected: Type,
    },
}

impl fmt::Display for ParamError {
    /// Render the error as a message naming the parameter and its declaration.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count { expected, supplied } => write!(
                f,
                "the statement has {expected} parameter{}, but {supplied} value{} supplied",
                if *expected == 1 { "" } else { "s" },
                if *supplied == 1 { " was" } else { "s were" },
            ),
            Self::Type {
                index,
                expected,
                value,
            } => write!(
                f,
                "parameter {index} is declared {}, but the value supplied is {value}",
                expected
            ),
            Self::Null { index, expected } => write!(
                f,
                "parameter {index} is declared {}, but NULL was supplied",
                expected
            ),
        }
    }
}

impl std::error::Error for ParamError {}

/// Collect the type each parameter of `rel` is declared with, by number.
///
/// The verifier rejects a plan that declares one parameter with two types,
/// so for a verified plan each number has one type; otherwise the first
/// declaration found wins.
///
/// # Examples
///
/// ```
/// use interlaken_ir::interp::declared_params;
/// use interlaken_ir::text::parse_plan as parse;
///
/// let plan = parse("(plan (filter (get t) (= x (param 2 int8))))")?;
/// let declared = declared_params(&plan.root);
/// assert_eq!(declared.keys().copied().collect::<Vec<_>>(), [2]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn declared_params(rel: &Rel) -> BTreeMap<u32, Type> {
    let mut out = BTreeMap::new();
    collect_rel(rel, &mut out);
    out
}

/// Add the parameters declared anywhere in `rel` to `out`.
fn collect_rel(rel: &Rel, out: &mut BTreeMap<u32, Type>) {
    for expr in rel.exprs() {
        collect_expr(expr, out);
    }
    for input in rel.inputs() {
        collect_rel(input, out);
    }
}

/// Add the parameters declared in `expr`, its subqueries included, to `out`.
fn collect_expr(expr: &Expr, out: &mut BTreeMap<u32, Type>) {
    match expr {
        Expr::Param { index, ty } => {
            out.entry(*index).or_insert_with(|| ty.clone());
        }
        Expr::Subquery { rel, .. } => collect_rel(rel, out),
        _ => {}
    }
    for child in expr.children() {
        collect_expr(child, out);
    }
}

/// Check `values` against the parameter types in `declared`.
///
/// There must be exactly one value per parameter number up to the highest
/// declared, and each declared parameter's value must be of its type:
/// the matching [`Datum`] variant, with array elements and record fields
/// checked in turn, and NULL only where the type allows it. Values for
/// numbers no parameter uses aren't checked.
///
/// # Errors
///
/// Returns [`ParamError::Count`] if there are too few or too many values,
/// [`ParamError::Null`] for NULL where the type forbids it, or
/// [`ParamError::Type`] for a value of another type.
///
/// # Examples
///
/// ```
/// use interlaken_ir::interp::{Datum, ParamError, check_params, declared_params};
/// use interlaken_ir::text::parse_plan as parse;
///
/// let plan = parse("(plan (filter (get t) (= x (param 1 (not-null int8)))))")?;
/// let declared = declared_params(&plan.root);
/// assert!(check_params(&declared, &[Datum::Int8(7)]).is_ok());
/// assert!(matches!(check_params(&declared, &[Datum::Null]), Err(ParamError::Null { .. })));
/// assert!(matches!(check_params(&declared, &[]), Err(ParamError::Count { .. })));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn check_params(declared: &BTreeMap<u32, Type>, values: &[Datum]) -> Result<(), ParamError> {
    let expected = declared
        .keys()
        .next_back()
        .map_or(0, |&n| usize::try_from(n).unwrap_or(usize::MAX));
    if values.len() != expected {
        return Err(ParamError::Count {
            expected,
            supplied: values.len(),
        });
    }
    for (&index, ty) in declared {
        let value = &values[usize::try_from(index).unwrap_or(usize::MAX) - 1];
        if value.is_null() && !ty.nullable {
            return Err(ParamError::Null {
                index,
                expected: ty.clone(),
            });
        }
        if !conforms(value, ty) {
            return Err(ParamError::Type {
                index,
                expected: ty.clone(),
                value: format!("{value:?}"),
            });
        }
    }
    Ok(())
}

/// Return whether `value` is a value of `ty`, NULLs included.
fn conforms(value: &Datum, ty: &Type) -> bool {
    use ScalarType as S;
    match (value, &ty.data) {
        (Datum::Null, _) => ty.nullable,
        (Datum::Bool(_), DataType::Scalar(S::Bool))
        | (Datum::Int2(_), DataType::Scalar(S::Int2))
        | (Datum::Int4(_), DataType::Scalar(S::Int4))
        | (Datum::Int8(_), DataType::Scalar(S::Int8))
        | (Datum::Float8(_), DataType::Scalar(S::Float8))
        | (Datum::Text(_), DataType::Scalar(S::Text))
        | (Datum::Bytea(_), DataType::Scalar(S::Bytea))
        | (Datum::Date(_), DataType::Scalar(S::Date))
        | (Datum::Timestamptz(_), DataType::Scalar(S::Timestamptz))
        | (Datum::Uuid(_), DataType::Scalar(S::Uuid))
        | (Datum::Jsonb(_), DataType::Scalar(S::Jsonb)) => true,
        (Datum::Array(items), DataType::Array(element)) => {
            items.iter().all(|item| conforms(item, element))
        }
        (Datum::Record(values), DataType::Record(fields)) => {
            values.len() == fields.len()
                && values.iter().zip(fields).all(|(v, f)| conforms(v, &f.ty))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::parse_plan as parse;

    /// Return the parameters declared by the plan text `plan`.
    fn declared(plan: &str) -> BTreeMap<u32, Type> {
        declared_params(&parse(plan).expect("plan parses").root)
    }

    /// Assert parameters are found in expressions, nested operators and subqueries.
    #[test]
    fn collects_parameters_everywhere() {
        let found = declared(
            "(plan (project (filter (get t :as a) (exists (filter (get u :as b) (= b.x (param 3 text)))))
               (+ a.x (param 1 int8))))",
        );
        assert_eq!(found.keys().copied().collect::<Vec<_>>(), [1, 3]);
        assert!(declared("(plan (get t))").is_empty());
    }

    /// Assert the count must match the highest declared number exactly.
    #[test]
    fn counts_must_match() {
        let found = declared("(plan (filter (get t) (= x (param 2 int8))))");
        // Parameter 1 isn't used, so any value fills its place.
        assert!(check_params(&found, &[Datum::Text("unused".into()), Datum::Int8(1)]).is_ok());
        assert_eq!(
            check_params(&found, &[Datum::Int8(1)]),
            Err(ParamError::Count {
                expected: 2,
                supplied: 1
            })
        );
        assert!(matches!(
            check_params(&found, &[Datum::Int8(1), Datum::Int8(2), Datum::Int8(3)]),
            Err(ParamError::Count { .. })
        ));
        assert!(check_params(&BTreeMap::new(), &[]).is_ok());
    }

    /// Assert values must have their declared type, recursively, and NULL only where allowed.
    #[test]
    fn values_must_conform() {
        let found = declared(
            "(plan (filter (get t) (and (= x (param 1 int8)) (= y (param 2 (array (not-null text)))))))",
        );
        let tags = |items: Vec<Datum>| Datum::Array(items);
        assert!(
            check_params(
                &found,
                &[Datum::Int8(1), tags(vec![Datum::Text("a".into())])]
            )
            .is_ok()
        );
        assert!(check_params(&found, &[Datum::Null, Datum::Null]).is_ok());
        let err = check_params(&found, &[Datum::Int4(1), Datum::Null]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "parameter 1 is declared int8, but the value supplied is Int4(1)"
        );
        assert!(matches!(
            check_params(&found, &[Datum::Int8(1), tags(vec![Datum::Null])]),
            Err(ParamError::Type { index: 2, .. })
        ));
        let strict = declared("(plan (filter (get t) (= x (param 1 (not-null int8)))))");
        assert_eq!(
            check_params(&strict, &[Datum::Null])
                .unwrap_err()
                .to_string(),
            "parameter 1 is declared (not-null int8), but NULL was supplied"
        );
    }
}
