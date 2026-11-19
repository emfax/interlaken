//! Column types of the minimal type system (ADR-0003, ADR-0007).

use super::Datum;

/// The type of a table column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnType {
    /// `bool`.
    Bool,
    /// `int2`.
    Int2,
    /// `int4`.
    Int4,
    /// `int8`.
    Int8,
    /// `float8`.
    Float8,
    /// `text`, binary (C) collation.
    Text,
    /// `bytea`.
    Bytea,
    /// A one-dimensional array of a scalar type, whose elements may be NULL.
    ///
    /// Arrays are values only: they can't be key or index columns.
    Array(ElementType),
}

/// The element type of an array column: any scalar column type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ElementType {
    /// `bool`.
    Bool,
    /// `int2`.
    Int2,
    /// `int4`.
    Int4,
    /// `int8`.
    Int8,
    /// `float8`.
    Float8,
    /// `text`.
    Text,
    /// `bytea`.
    Bytea,
}

impl ElementType {
    /// Return the scalar column type of the elements.
    #[must_use]
    pub const fn column_type(self) -> ColumnType {
        match self {
            Self::Bool => ColumnType::Bool,
            Self::Int2 => ColumnType::Int2,
            Self::Int4 => ColumnType::Int4,
            Self::Int8 => ColumnType::Int8,
            Self::Float8 => ColumnType::Float8,
            Self::Text => ColumnType::Text,
            Self::Bytea => ColumnType::Bytea,
        }
    }

    /// Return the element type for a scalar column type, or `None` for an array type.
    #[must_use]
    pub const fn of(ty: ColumnType) -> Option<Self> {
        Some(match ty {
            ColumnType::Bool => Self::Bool,
            ColumnType::Int2 => Self::Int2,
            ColumnType::Int4 => Self::Int4,
            ColumnType::Int8 => Self::Int8,
            ColumnType::Float8 => Self::Float8,
            ColumnType::Text => Self::Text,
            ColumnType::Bytea => Self::Bytea,
            ColumnType::Array(_) => return None,
        })
    }
}

/// Catalog codes of array types are this plus the element type's code.
const ARRAY_CODE_BASE: i16 = 100;

impl ColumnType {
    /// Every scalar column type, in code order. Each also has an array type.
    pub const ALL: [Self; 7] = [
        Self::Bool,
        Self::Int2,
        Self::Int4,
        Self::Int8,
        Self::Float8,
        Self::Text,
        Self::Bytea,
    ];

    /// Return the stored width of a non-NULL value, or `None` for variable-length types.
    #[must_use]
    pub const fn fixed_width(self) -> Option<usize> {
        match self {
            Self::Bool => Some(1),
            Self::Int2 => Some(2),
            Self::Int4 => Some(4),
            Self::Int8 | Self::Float8 => Some(8),
            Self::Text | Self::Bytea | Self::Array(_) => None,
        }
    }

    /// Return `true` if the type can be a key or index column (every scalar type).
    #[must_use]
    pub const fn is_keyable(self) -> bool {
        !matches!(self, Self::Array(_))
    }

    /// Return `true` if `datum` is NULL or a value of this type.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_storage::encoding::{ColumnType, Datum};
    ///
    /// assert!(ColumnType::Int4.accepts(&Datum::Int4(7)));
    /// assert!(ColumnType::Int4.accepts(&Datum::Null));
    /// assert!(!ColumnType::Int4.accepts(&Datum::Int8(7)));
    /// ```
    #[must_use]
    pub fn accepts(self, datum: &Datum) -> bool {
        if let (Self::Array(element), Datum::Array(items)) = (self, datum) {
            let element = element.column_type();
            return items.iter().all(|item| element.accepts(item));
        }
        matches!(
            (self, datum),
            (_, Datum::Null)
                | (Self::Bool, Datum::Bool(_))
                | (Self::Int2, Datum::Int2(_))
                | (Self::Int4, Datum::Int4(_))
                | (Self::Int8, Datum::Int8(_))
                | (Self::Float8, Datum::Float8(_))
                | (Self::Text, Datum::Text(_))
                | (Self::Bytea, Datum::Bytea(_))
        )
    }

    /// Return the stable code stored in the catalog.
    #[must_use]
    pub const fn code(self) -> i16 {
        match self {
            Self::Bool => 1,
            Self::Int2 => 2,
            Self::Int4 => 3,
            Self::Int8 => 4,
            Self::Float8 => 5,
            Self::Text => 6,
            Self::Bytea => 7,
            Self::Array(element) => ARRAY_CODE_BASE + element.column_type().code(),
        }
    }

    /// Return the type with catalog code `code`, if any.
    #[must_use]
    pub fn from_code(code: i16) -> Option<Self> {
        if let Some(scalar) = Self::ALL.into_iter().find(|ty| ty.code() == code) {
            return Some(scalar);
        }
        let element = Self::from_code(code.checked_sub(ARRAY_CODE_BASE)?)?;
        ElementType::of(element).map(Self::Array)
    }
}

impl std::fmt::Display for ColumnType {
    /// Render the Postgres type name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Self::Array(element) = self {
            return write!(f, "{}[]", element.column_type());
        }
        f.write_str(match self {
            Self::Bool => "bool",
            Self::Int2 => "int2",
            Self::Int4 => "int4",
            Self::Int8 => "int8",
            Self::Float8 => "float8",
            Self::Text => "text",
            Self::Bytea => "bytea",
            Self::Array(_) => unreachable!("arrays are rendered above"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert catalog codes round-trip and are distinct.
    fn codes_round_trip() {
        let mut codes = std::collections::HashSet::new();
        for scalar in ColumnType::ALL {
            let array = ColumnType::Array(ElementType::of(scalar).unwrap());
            for ty in [scalar, array] {
                assert_eq!(ColumnType::from_code(ty.code()), Some(ty));
                assert!(codes.insert(ty.code()), "{ty} repeats a code");
            }
            assert_eq!(array.to_string(), format!("{scalar}[]"));
            assert!(scalar.is_keyable() && !array.is_keyable());
        }
        for code in [0, 100, 200, 207, -1] {
            assert_eq!(ColumnType::from_code(code), None, "{code}");
        }
    }
}
