//! Comparison kernels: `=`, `<>`, `<`, `<=`, `>`, `>=` and `IS [NOT] DISTINCT FROM`.
//!
//! Orderings follow the interpreter: floats order NaN above everything and
//! equal to itself, with -0 equal to 0; text and bytes compare bytewise
//! (C collation); arrays and records compare element by element.

use std::cmp::Ordering;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Float64Type, Int16Type, Int32Type, Int64Type, TimestampMicrosecondType,
};
use arrow_array::{Array, ArrayAccessor, ArrayRef, BooleanArray};
use arrow_schema::{DataType as ArrowType, TimeUnit};
use interlaken_ir::BinaryOp;
use interlaken_ir::interp::EvalError;

use crate::columnar::datum_at;
use crate::error::ExecError;

/// Compare floats as Postgres does: NaN equals NaN and sorts above everything; -0 equals 0.
pub(crate) fn compare_floats(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).expect("neither is NaN"),
    }
}

/// Return the ordering of each row's pair of values; `None` where either is NULL.
fn ordered<A: ArrayAccessor>(
    a: A,
    b: A,
    cmp: impl Fn(A::Item, A::Item) -> Ordering,
) -> Vec<Option<Ordering>> {
    (0..a.len())
        .map(|i| (a.is_valid(i) && b.is_valid(i)).then(|| cmp(a.value(i), b.value(i))))
        .collect()
}

/// Return the ordering of each row's values in `a` and `b`, which have the same type.
fn orderings(a: &ArrayRef, b: &ArrayRef) -> Result<Vec<Option<Ordering>>, ExecError> {
    if a.data_type() != b.data_type() || a.len() != b.len() {
        return Err(ExecError::Unsupported(format!(
            "comparing {} with {}",
            a.data_type(),
            b.data_type()
        )));
    }
    Ok(match a.data_type() {
        ArrowType::Boolean => ordered(a.as_boolean(), b.as_boolean(), |x, y| x.cmp(&y)),
        ArrowType::Int16 => ordered(
            a.as_primitive::<Int16Type>(),
            b.as_primitive::<Int16Type>(),
            |x, y| x.cmp(&y),
        ),
        ArrowType::Int32 => ordered(
            a.as_primitive::<Int32Type>(),
            b.as_primitive::<Int32Type>(),
            |x, y| x.cmp(&y),
        ),
        ArrowType::Int64 => ordered(
            a.as_primitive::<Int64Type>(),
            b.as_primitive::<Int64Type>(),
            |x, y| x.cmp(&y),
        ),
        ArrowType::Date32 => ordered(
            a.as_primitive::<Date32Type>(),
            b.as_primitive::<Date32Type>(),
            |x, y| x.cmp(&y),
        ),
        ArrowType::Timestamp(TimeUnit::Microsecond, _) => ordered(
            a.as_primitive::<TimestampMicrosecondType>(),
            b.as_primitive::<TimestampMicrosecondType>(),
            |x, y| x.cmp(&y),
        ),
        ArrowType::Float64 => ordered(
            a.as_primitive::<Float64Type>(),
            b.as_primitive::<Float64Type>(),
            compare_floats,
        ),
        ArrowType::Utf8 => ordered(a.as_string::<i32>(), b.as_string::<i32>(), |x, y| {
            x.as_bytes().cmp(y.as_bytes())
        }),
        ArrowType::Binary => ordered(a.as_binary::<i32>(), b.as_binary::<i32>(), |x, y| x.cmp(y)),
        ArrowType::FixedSizeBinary(_) => ordered(
            a.as_fixed_size_binary(),
            b.as_fixed_size_binary(),
            |x, y| x.cmp(y),
        ),
        _ => (0..a.len())
            .map(|i| {
                let (x, y) = (datum_at(a.as_ref(), i)?, datum_at(b.as_ref(), i)?);
                if x.is_null() || y.is_null() {
                    return Ok(None);
                }
                x.compare(&y).map(Some).ok_or_else(|| {
                    EvalError::Unsupported(format!("comparing {x:?} with {y:?}")).into()
                })
            })
            .collect::<Result<_, ExecError>>()?,
    })
}

/// Apply a comparison operator; NULL where either side is NULL.
pub(crate) fn compare(op: BinaryOp, a: &ArrayRef, b: &ArrayRef) -> Result<ArrayRef, ExecError> {
    let test: fn(Ordering) -> bool = match op {
        BinaryOp::Eq => Ordering::is_eq,
        BinaryOp::Ne => Ordering::is_ne,
        BinaryOp::Lt => Ordering::is_lt,
        BinaryOp::Le => Ordering::is_le,
        BinaryOp::Gt => Ordering::is_gt,
        BinaryOp::Ge => Ordering::is_ge,
        other => {
            return Err(ExecError::Unsupported(format!(
                "{other:?} is not a comparison"
            )));
        }
    };
    let result: BooleanArray = orderings(a, b)?.into_iter().map(|o| o.map(test)).collect();
    Ok(Arc::new(result))
}

/// Apply `IS DISTINCT FROM` (`distinct` true) or `IS NOT DISTINCT FROM`; never NULL.
pub(crate) fn distinct(a: &ArrayRef, b: &ArrayRef, distinct: bool) -> Result<ArrayRef, ExecError> {
    let orderings = orderings(a, b)?;
    let result: BooleanArray = orderings
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let same = match (a.is_null(i), b.is_null(i)) {
                (true, true) => true,
                (false, false) => *o == Some(Ordering::Equal),
                _ => false,
            };
            Some(same != distinct)
        })
        .collect();
    Ok(Arc::new(result))
}
