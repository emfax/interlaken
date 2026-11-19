//! Arithmetic kernels with Postgres's error semantics.
//!
//! Integer results that do not fit their type raise "out of range" rather
//! than wrapping; division and modulo by zero raise; float results that
//! overflow to infinity (or underflow to zero) from finite operands raise.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    ArrowPrimitiveType, Date32Type, Float64Type, Int16Type, Int32Type, Int64Type,
};
use arrow_array::{Array, ArrayRef, PrimitiveArray};
use arrow_schema::DataType as ArrowType;
use interlaken_ir::BinaryOp;
use interlaken_ir::interp::EvalError;

use crate::error::ExecError;

/// Apply `f` to each non-NULL value, keeping NULLs.
pub(crate) fn map<I: ArrowPrimitiveType, O: ArrowPrimitiveType>(
    a: &PrimitiveArray<I>,
    mut f: impl FnMut(I::Native) -> Result<O::Native, EvalError>,
) -> Result<PrimitiveArray<O>, EvalError> {
    a.iter().map(|v| v.map(&mut f).transpose()).collect()
}

/// Apply `f` to each pair of non-NULL values; NULL where either is NULL.
pub(crate) fn zip<A: ArrowPrimitiveType, B: ArrowPrimitiveType, O: ArrowPrimitiveType>(
    a: &PrimitiveArray<A>,
    b: &PrimitiveArray<B>,
    mut f: impl FnMut(A::Native, B::Native) -> Result<O::Native, EvalError>,
) -> Result<PrimitiveArray<O>, EvalError> {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| match (x, y) {
            (Some(x), Some(y)) => f(x, y).map(Some),
            _ => Ok(None),
        })
        .collect()
}

/// Return the out-of-range error for an integer Arrow type.
pub(crate) fn int_range_error(ty: &ArrowType) -> EvalError {
    EvalError::OutOfRange(
        match ty {
            ArrowType::Int16 => "smallint",
            ArrowType::Int32 => "integer",
            _ => "bigint",
        }
        .into(),
    )
}

/// Narrow a wide result to the native type `T`, or raise `error`.
pub(crate) fn narrow<T: TryFrom<i128>>(v: i128, error: &EvalError) -> Result<T, EvalError> {
    T::try_from(v).map_err(|_| error.clone())
}

/// Apply integer arithmetic in `T`, computing exactly and then narrowing.
fn integers<T>(op: BinaryOp, a: &ArrayRef, b: &ArrayRef) -> Result<ArrayRef, ExecError>
where
    T: ArrowPrimitiveType,
    T::Native: Into<i128> + TryFrom<i128>,
{
    let range = int_range_error(a.data_type());
    let result = zip::<T, T, T>(a.as_primitive::<T>(), b.as_primitive::<T>(), |x, y| {
        let (x, y): (i128, i128) = (x.into(), y.into());
        let wide = match op {
            BinaryOp::Add => x + y,
            BinaryOp::Sub => x - y,
            BinaryOp::Mul => x * y,
            BinaryOp::Div | BinaryOp::Mod if y == 0 => return Err(EvalError::DivisionByZero),
            BinaryOp::Div => x / y,
            _ => x % y,
        };
        narrow(wide, &range)
    })?;
    Ok(Arc::new(result))
}

/// Check a float result for overflow and underflow as Postgres does.
fn checked_float(result: f64, a: f64, b: f64, zero_ok: bool) -> Result<f64, EvalError> {
    if result.is_infinite() && a.is_finite() && b.is_finite() {
        return Err(EvalError::OutOfRange("value out of range: overflow".into()));
    }
    if result == 0.0 && !zero_ok {
        return Err(EvalError::OutOfRange(
            "value out of range: underflow".into(),
        ));
    }
    Ok(result)
}

/// Apply float arithmetic.
fn floats(op: BinaryOp, a: &ArrayRef, b: &ArrayRef) -> Result<ArrayRef, ExecError> {
    let result = zip::<Float64Type, Float64Type, Float64Type>(
        a.as_primitive(),
        b.as_primitive(),
        |x, y| match op {
            BinaryOp::Add => checked_float(x + y, x, y, true),
            BinaryOp::Sub => checked_float(x - y, x, y, true),
            BinaryOp::Mul => checked_float(x * y, x, y, x == 0.0 || y == 0.0),
            BinaryOp::Div if y == 0.0 => Err(EvalError::DivisionByZero),
            BinaryOp::Div => checked_float(x / y, x, y, x == 0.0 || y.is_infinite()),
            other => Err(EvalError::Unsupported(format!("{other:?} on float8"))),
        },
    )?;
    Ok(Arc::new(result))
}

/// Apply an arithmetic operator.
pub(crate) fn arithmetic(op: BinaryOp, a: &ArrayRef, b: &ArrayRef) -> Result<ArrayRef, ExecError> {
    let date_range = || EvalError::OutOfRange("date".into());
    match (a.data_type(), b.data_type()) {
        (ArrowType::Int16, ArrowType::Int16) => integers::<Int16Type>(op, a, b),
        (ArrowType::Int32, ArrowType::Int32) => integers::<Int32Type>(op, a, b),
        (ArrowType::Int64, ArrowType::Int64) => integers::<Int64Type>(op, a, b),
        (ArrowType::Float64, ArrowType::Float64) => floats(op, a, b),
        (ArrowType::Date32, ArrowType::Int32) if matches!(op, BinaryOp::Add | BinaryOp::Sub) => {
            let result = zip::<Date32Type, Int32Type, Date32Type>(
                a.as_primitive(),
                b.as_primitive(),
                |d, n| {
                    if op == BinaryOp::Add {
                        d.checked_add(n)
                    } else {
                        d.checked_sub(n)
                    }
                    .ok_or_else(date_range)
                },
            )?;
            Ok(Arc::new(result))
        }
        (ArrowType::Date32, ArrowType::Date32) if op == BinaryOp::Sub => {
            let range = int_range_error(&ArrowType::Int32);
            let result = zip::<Date32Type, Date32Type, Int32Type>(
                a.as_primitive(),
                b.as_primitive(),
                |x, y| x.checked_sub(y).ok_or_else(|| range.clone()),
            )?;
            Ok(Arc::new(result))
        }
        (x, y) => Err(EvalError::Unsupported(format!("{op:?} on {x} and {y}")).into()),
    }
}

/// Negate integers (raising on overflow) or floats.
pub(crate) fn negate(a: &ArrayRef) -> Result<ArrayRef, ExecError> {
    /// Negate integers of type `T`.
    fn ints<T>(a: &ArrayRef) -> Result<ArrayRef, ExecError>
    where
        T: ArrowPrimitiveType,
        T::Native: Into<i128> + TryFrom<i128>,
    {
        let range = int_range_error(a.data_type());
        Ok(Arc::new(map::<T, T>(a.as_primitive::<T>(), |x| {
            narrow(-x.into(), &range)
        })?))
    }
    match a.data_type() {
        ArrowType::Int16 => ints::<Int16Type>(a),
        ArrowType::Int32 => ints::<Int32Type>(a),
        ArrowType::Int64 => ints::<Int64Type>(a),
        ArrowType::Float64 => Ok(Arc::new(map::<Float64Type, Float64Type>(
            a.as_primitive(),
            |x| Ok(-x),
        )?)),
        other => Err(EvalError::Unsupported(format!("negating {other}")).into()),
    }
}

/// Return the absolute value of integers (raising on overflow) or floats.
pub(crate) fn abs(a: &ArrayRef) -> Result<ArrayRef, ExecError> {
    /// Take absolute values of integers of type `T`.
    fn ints<T>(a: &ArrayRef) -> Result<ArrayRef, ExecError>
    where
        T: ArrowPrimitiveType,
        T::Native: Into<i128> + TryFrom<i128>,
    {
        let range = int_range_error(a.data_type());
        Ok(Arc::new(map::<T, T>(a.as_primitive::<T>(), |x| {
            narrow(x.into().abs(), &range)
        })?))
    }
    match a.data_type() {
        ArrowType::Int16 => ints::<Int16Type>(a),
        ArrowType::Int32 => ints::<Int32Type>(a),
        ArrowType::Int64 => ints::<Int64Type>(a),
        ArrowType::Float64 => Ok(Arc::new(map::<Float64Type, Float64Type>(
            a.as_primitive(),
            |x| Ok(x.abs()),
        )?)),
        other => Err(EvalError::Unsupported(format!("abs of {other}")).into()),
    }
}
