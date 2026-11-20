//! Mapping IR column types to storage column types and back.

use interlaken_ir::{DataType, ScalarType, Type};
use interlaken_storage::encoding::ColumnType;

use crate::error::EngineError;

/// Return the storage type that holds column type `ty`.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] for types tables can't hold
/// (records, entity references, nested arrays).
pub(crate) fn stored_type(column: &str, ty: &Type) -> Result<ColumnType, EngineError> {
    interlaken_exec::stored_type(&ty.data).ok_or_else(|| {
        EngineError::InvalidDefinition(format!(
            "column \"{column}\" has type {}, which tables can't hold",
            interlaken_ir::text::print_type(ty)
        ))
    })
}

/// Return the IR type of a stored column whose IR type was not recorded:
/// the storage type's namesake, with array elements nullable as storage allows.
pub(crate) fn derived_type(ty: ColumnType, nullable: bool) -> Type {
    let data = match ty {
        ColumnType::Array(element) => {
            DataType::Array(Box::new(derived_type(element.column_type(), true)))
        }
        scalar => DataType::Scalar(scalar_type(scalar)),
    };
    Type { data, nullable }
}

/// Return the IR scalar type of a scalar storage type.
fn scalar_type(ty: ColumnType) -> ScalarType {
    match ty {
        ColumnType::Bool => ScalarType::Bool,
        ColumnType::Int2 => ScalarType::Int2,
        ColumnType::Int4 => ScalarType::Int4,
        ColumnType::Int8 => ScalarType::Int8,
        ColumnType::Float8 => ScalarType::Float8,
        ColumnType::Bytea => ScalarType::Bytea,
        ColumnType::Date => ScalarType::Date,
        ColumnType::Timestamptz => ScalarType::Timestamptz,
        ColumnType::Uuid => ScalarType::Uuid,
        ColumnType::Jsonb => ScalarType::Jsonb,
        // Text, and arrays (handled by the caller); unknown future types read as text.
        _ => ScalarType::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert every storable IR type maps to a storage type whose derived IR type matches it.
    fn round_trip() {
        for scalar in ScalarType::ALL {
            for ty in [
                DataType::Scalar(scalar).not_null(),
                DataType::Array(Box::new(DataType::Scalar(scalar).nullable())).nullable(),
            ] {
                let stored = stored_type("c", &ty).unwrap();
                assert_eq!(derived_type(stored, ty.nullable), ty);
            }
        }
        let record = DataType::Record(Vec::new()).nullable();
        assert!(stored_type("c", &record).is_err());
    }
}
