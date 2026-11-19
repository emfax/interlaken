//! Cast kernels.
//!
//! Numeric, boolean and date/time conversions run natively; conversions to
//! and from text (and anything else) apply the interpreter's
//! [`cast_value`] to each value, so parsing and formatting have one
//! definition.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    ArrowPrimitiveType, Date32Type, Float64Type, Int16Type, Int32Type, Int64Type,
    TimestampMicrosecondType,
};
use arrow_array::{Array, ArrayRef, BooleanArray, PrimitiveArray};
use interlaken_ir::interp::{EvalError, cast_value};
use interlaken_ir::{DataType, ScalarType};

use super::arith::{int_range_error, map, narrow};
use crate::columnar::{UTC, arrow_type, build_array, datum_at};
use crate::error::ExecError;

/// Microseconds in a day.
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Return the integer values of an integer array, widened.
fn wide_ints(a: &ArrayRef) -> Option<Vec<Option<i128>>> {
    Some(match a.data_type() {
        arrow_schema::DataType::Int16 => a
            .as_primitive::<Int16Type>()
            .iter()
            .map(|v| v.map(i128::from))
            .collect(),
        arrow_schema::DataType::Int32 => a
            .as_primitive::<Int32Type>()
            .iter()
            .map(|v| v.map(i128::from))
            .collect(),
        arrow_schema::DataType::Int64 => a
            .as_primitive::<Int64Type>()
            .iter()
            .map(|v| v.map(i128::from))
            .collect(),
        _ => return None,
    })
}

/// Build an integer array of type `T` from wide values, raising if one does not fit.
fn narrow_ints<T>(values: &[Option<i128>]) -> Result<ArrayRef, ExecError>
where
    T: ArrowPrimitiveType,
    T::Native: TryFrom<i128>,
{
    let range = int_range_error(&T::DATA_TYPE);
    let result: PrimitiveArray<T> = values
        .iter()
        .map(|v| v.map(|v| narrow::<T::Native>(v, &range)).transpose())
        .collect::<Result<_, _>>()?;
    Ok(Arc::new(result))
}

/// Build an integer array of the integer type `to` from wide values.
fn to_int(values: &[Option<i128>], to: ScalarType) -> Result<ArrayRef, ExecError> {
    match to {
        ScalarType::Int2 => narrow_ints::<Int16Type>(values),
        ScalarType::Int4 => narrow_ints::<Int32Type>(values),
        _ => narrow_ints::<Int64Type>(values),
    }
}

/// Cast `a`, of type `from`, to `to`.
pub(crate) fn cast(a: &ArrayRef, from: &DataType, to: &DataType) -> Result<ArrayRef, ExecError> {
    use ScalarType as S;
    if from == to || !matches!(to, DataType::Scalar(_)) {
        return Ok(a.clone());
    }
    let (DataType::Scalar(source), DataType::Scalar(target)) = (from, to) else {
        return per_value(a, to);
    };
    let is_int = |t: &S| matches!(t, S::Int2 | S::Int4 | S::Int8);
    match (source, target) {
        (s, t) if is_int(s) && is_int(t) => to_int(&wide_ints(a).expect("integer array"), *t),
        (s, S::Float8) if is_int(s) => {
            #[allow(clippy::cast_precision_loss)]
            let result: PrimitiveArray<Float64Type> = wide_ints(a)
                .expect("integer array")
                .into_iter()
                .map(|v| v.map(|v| v as f64))
                .collect();
            Ok(Arc::new(result))
        }
        (S::Float8, t) if is_int(t) => {
            let range = int_range_error(&arrow_type(to));
            let values = a
                .as_primitive::<Float64Type>()
                .iter()
                .map(|v| {
                    v.map(|v| {
                        // Postgres rounds half to even (rint).
                        let rounded = v.round_ties_even();
                        if !rounded.is_finite() || rounded.abs() >= 2f64.powi(63) {
                            return Err(range.clone());
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        Ok(i128::from(rounded as i64))
                    })
                    .transpose()
                })
                .collect::<Result<Vec<_>, _>>()?;
            to_int(&values, *t)
        }
        (s, S::Bool) if is_int(s) => {
            let result: BooleanArray = wide_ints(a)
                .expect("integer array")
                .into_iter()
                .map(|v| v.map(|v| v != 0))
                .collect();
            Ok(Arc::new(result))
        }
        (S::Bool, t) if is_int(t) => {
            let values: Vec<Option<i128>> =
                a.as_boolean().iter().map(|b| b.map(i128::from)).collect();
            to_int(&values, *t)
        }
        (S::Date, S::Timestamptz) => {
            let result = map::<Date32Type, TimestampMicrosecondType>(a.as_primitive(), |days| {
                i64::from(days)
                    .checked_mul(MICROS_PER_DAY)
                    .ok_or_else(|| EvalError::OutOfRange("timestamp".into()))
            })?;
            Ok(Arc::new(result.with_timezone(UTC)))
        }
        (S::Timestamptz, S::Date) => {
            let result = map::<TimestampMicrosecondType, Date32Type>(a.as_primitive(), |micros| {
                i32::try_from(micros.div_euclid(MICROS_PER_DAY))
                    .map_err(|_| EvalError::OutOfRange("date".into()))
            })?;
            Ok(Arc::new(result))
        }
        _ => per_value(a, to),
    }
}

/// Cast each value with the interpreter's scalar cast.
fn per_value(a: &ArrayRef, to: &DataType) -> Result<ArrayRef, ExecError> {
    let values = (0..a.len())
        .map(|i| Ok(cast_value(datum_at(a.as_ref(), i)?, to)?))
        .collect::<Result<Vec<_>, ExecError>>()?;
    build_array(to, &values)
}
