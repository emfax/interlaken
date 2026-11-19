//! Converting between IR types and values and Arrow arrays.

use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, FixedSizeBinaryBuilder, Float64Builder,
    Int16Builder, Int32Builder, Int64Builder, StringBuilder, TimestampMicrosecondBuilder,
};
use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Float64Type, Int16Type, Int32Type, Int64Type, TimestampMicrosecondType,
};
use arrow_array::{Array, ArrayRef, ListArray, StructArray};
use arrow_buffer::{OffsetBuffer, ScalarBuffer};
use arrow_schema::{DataType as ArrowType, Field, Fields, TimeUnit};
use interlaken_ir::interp::Datum;
use interlaken_ir::{DataType, ScalarType, Type};

use crate::error::ExecError;

/// The time zone of every `timestamptz` column.
const UTC: &str = "+00:00";

/// Return the Arrow type of an IR data type.
#[must_use]
pub fn arrow_type(ty: &DataType) -> ArrowType {
    match ty {
        DataType::Scalar(s) => match s {
            ScalarType::Bool => ArrowType::Boolean,
            ScalarType::Int2 => ArrowType::Int16,
            ScalarType::Int4 => ArrowType::Int32,
            ScalarType::Int8 => ArrowType::Int64,
            ScalarType::Float8 => ArrowType::Float64,
            ScalarType::Text | ScalarType::Jsonb => ArrowType::Utf8,
            ScalarType::Bytea => ArrowType::Binary,
            ScalarType::Date => ArrowType::Date32,
            ScalarType::Timestamptz => {
                ArrowType::Timestamp(TimeUnit::Microsecond, Some(UTC.into()))
            }
            ScalarType::Uuid => ArrowType::FixedSizeBinary(16),
        },
        DataType::Array(element) => ArrowType::List(Arc::new(field("item", element))),
        DataType::Record(fields) => ArrowType::Struct(record_fields(fields)),
        DataType::Entity(_) => ArrowType::Null,
    }
}

/// Return the Arrow field for an IR type.
#[must_use]
pub fn field(name: &str, ty: &Type) -> Field {
    Field::new(name, arrow_type(&ty.data), ty.nullable)
}

/// Return the Arrow fields of a record type.
fn record_fields(fields: &[interlaken_ir::Field]) -> Fields {
    fields
        .iter()
        .map(|f| field(f.name.as_str(), &f.ty))
        .collect()
}

/// Construct a type-mismatch error.
fn mismatch(ty: &DataType, d: &Datum) -> ExecError {
    ExecError::Unsupported(format!("value {d:?} in a column of type {ty}"))
}

/// Build an Arrow array of type `ty` from values.
///
/// # Errors
///
/// Returns [`ExecError::Unsupported`] if a value does not match `ty`.
pub fn build_array(ty: &DataType, values: &[Datum]) -> Result<ArrayRef, ExecError> {
    /// Append scalar values with a typed builder.
    macro_rules! scalar {
        ($builder:expr, $variant:ident) => {{
            let mut b = $builder;
            for v in values {
                match v {
                    Datum::Null => b.append_null(),
                    Datum::$variant(x) => b.append_value(x.clone()),
                    other => return Err(mismatch(ty, other)),
                }
            }
            Arc::new(b.finish()) as ArrayRef
        }};
    }
    Ok(match ty {
        DataType::Scalar(ScalarType::Bool) => scalar!(BooleanBuilder::new(), Bool),
        DataType::Scalar(ScalarType::Int2) => scalar!(Int16Builder::new(), Int2),
        DataType::Scalar(ScalarType::Int4) => scalar!(Int32Builder::new(), Int4),
        DataType::Scalar(ScalarType::Int8) => scalar!(Int64Builder::new(), Int8),
        DataType::Scalar(ScalarType::Float8) => scalar!(Float64Builder::new(), Float8),
        DataType::Scalar(ScalarType::Text) => scalar!(StringBuilder::new(), Text),
        DataType::Scalar(ScalarType::Jsonb) => scalar!(StringBuilder::new(), Jsonb),
        DataType::Scalar(ScalarType::Bytea) => scalar!(BinaryBuilder::new(), Bytea),
        DataType::Scalar(ScalarType::Date) => scalar!(Date32Builder::new(), Date),
        DataType::Scalar(ScalarType::Timestamptz) => {
            let array = scalar!(TimestampMicrosecondBuilder::new(), Timestamptz);
            Arc::new(
                array
                    .as_primitive::<TimestampMicrosecondType>()
                    .clone()
                    .with_timezone(UTC),
            )
        }
        DataType::Scalar(ScalarType::Uuid) => {
            let mut b = FixedSizeBinaryBuilder::new(16);
            for v in values {
                match v {
                    Datum::Null => b.append_null(),
                    Datum::Uuid(x) => b.append_value(x)?,
                    other => return Err(mismatch(ty, other)),
                }
            }
            Arc::new(b.finish())
        }
        DataType::Array(element) => {
            let mut offsets = vec![0i32];
            let mut children = Vec::new();
            let mut nulls = Vec::new();
            for v in values {
                match v {
                    Datum::Null => nulls.push(false),
                    Datum::Array(items) => {
                        children.extend(items.iter().cloned());
                        nulls.push(true);
                    }
                    other => return Err(mismatch(ty, other)),
                }
                offsets.push(
                    i32::try_from(children.len())
                        .map_err(|_| ExecError::Unsupported("list too long".into()))?,
                );
            }
            let child = build_array(&element.data, &children)?;
            Arc::new(ListArray::try_new(
                Arc::new(field("item", element)),
                OffsetBuffer::new(ScalarBuffer::from(offsets)),
                child,
                Some(nulls.into()),
            )?)
        }
        DataType::Record(fields) => {
            let mut columns: Vec<Vec<Datum>> = vec![Vec::with_capacity(values.len()); fields.len()];
            let mut nulls = Vec::new();
            for v in values {
                match v {
                    Datum::Null => {
                        nulls.push(false);
                        columns.iter_mut().for_each(|c| c.push(Datum::Null));
                    }
                    Datum::Record(items) if items.len() == fields.len() => {
                        nulls.push(true);
                        for (c, item) in columns.iter_mut().zip(items) {
                            c.push(item.clone());
                        }
                    }
                    other => return Err(mismatch(ty, other)),
                }
            }
            let arrays = fields
                .iter()
                .zip(&columns)
                .map(|(f, c)| build_array(&f.ty.data, c))
                .collect::<Result<Vec<_>, _>>()?;
            Arc::new(StructArray::try_new(
                record_fields(fields),
                arrays,
                Some(nulls.into()),
            )?)
        }
        DataType::Entity(_) => return Err(ExecError::Unsupported("entity values".into())),
    })
}

/// Read the value at `row` of `array`.
///
/// # Errors
///
/// Returns [`ExecError::Unsupported`] for an Arrow type the IR does not use.
pub fn datum_at(array: &dyn Array, row: usize) -> Result<Datum, ExecError> {
    if array.is_null(row) {
        return Ok(Datum::Null);
    }
    Ok(match array.data_type() {
        ArrowType::Boolean => Datum::Bool(array.as_boolean().value(row)),
        ArrowType::Int16 => Datum::Int2(array.as_primitive::<Int16Type>().value(row)),
        ArrowType::Int32 => Datum::Int4(array.as_primitive::<Int32Type>().value(row)),
        ArrowType::Int64 => Datum::Int8(array.as_primitive::<Int64Type>().value(row)),
        ArrowType::Float64 => Datum::Float8(array.as_primitive::<Float64Type>().value(row)),
        ArrowType::Utf8 => Datum::Text(array.as_string::<i32>().value(row).to_owned()),
        ArrowType::Binary => Datum::Bytea(array.as_binary::<i32>().value(row).to_vec()),
        ArrowType::Date32 => Datum::Date(array.as_primitive::<Date32Type>().value(row)),
        ArrowType::Timestamp(TimeUnit::Microsecond, _) => {
            Datum::Timestamptz(array.as_primitive::<TimestampMicrosecondType>().value(row))
        }
        ArrowType::FixedSizeBinary(16) => {
            let bytes = array.as_fixed_size_binary().value(row);
            Datum::Uuid(
                bytes
                    .try_into()
                    .map_err(|_| ExecError::Unsupported("uuid width".into()))?,
            )
        }
        ArrowType::List(_) => {
            let list = array.as_list::<i32>().value(row);
            Datum::Array(
                (0..list.len())
                    .map(|i| datum_at(list.as_ref(), i))
                    .collect::<Result<_, _>>()?,
            )
        }
        ArrowType::Struct(_) => {
            let s = array.as_struct();
            Datum::Record(
                s.columns()
                    .iter()
                    .map(|c| datum_at(c.as_ref(), row))
                    .collect::<Result<_, _>>()?,
            )
        }
        other => return Err(ExecError::Unsupported(format!("Arrow type {other}"))),
    })
}
