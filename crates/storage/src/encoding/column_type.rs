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
}

impl ColumnType {
    /// Every column type, in code order.
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
            Self::Text | Self::Bytea => None,
        }
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
        }
    }

    /// Return the type with catalog code `code`, if any.
    #[must_use]
    pub fn from_code(code: i16) -> Option<Self> {
        Self::ALL.into_iter().find(|ty| ty.code() == code)
    }
}

impl std::fmt::Display for ColumnType {
    /// Render the Postgres type name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Bool => "bool",
            Self::Int2 => "int2",
            Self::Int4 => "int4",
            Self::Int8 => "int8",
            Self::Float8 => "float8",
            Self::Text => "text",
            Self::Bytea => "bytea",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert catalog codes round-trip and are distinct.
    fn codes_round_trip() {
        for ty in ColumnType::ALL {
            assert_eq!(ColumnType::from_code(ty.code()), Some(ty));
        }
        assert_eq!(ColumnType::from_code(0), None);
    }
}
