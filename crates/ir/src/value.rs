//! Literal values (ADR-0010).

use std::hash::{Hash, Hasher};

use crate::types::ScalarType;

/// A literal scalar value.
///
/// `Null` is untyped: resolved IR wraps a NULL that needs a type in an
/// explicit cast. Floats compare and hash by bit pattern, except that all
/// NaNs are one value (as in Postgres): `NaN` equals itself and `0.0`
/// differs from `-0.0`. This is structural equality of IR nodes, not SQL
/// comparison.
#[derive(Clone, Debug)]
pub enum Value {
    /// SQL NULL.
    Null,
    /// A `bool`.
    Bool(bool),
    /// An `int2`.
    Int2(i16),
    /// An `int4`.
    Int4(i32),
    /// An `int8`.
    Int8(i64),
    /// A `float8`.
    Float8(f64),
    /// A `text` value.
    Text(String),
    /// A `bytea` value.
    Bytea(Vec<u8>),
    /// A `date`: days since 1970-01-01.
    Date(i32),
    /// A `timestamptz`: microseconds since 1970-01-01T00:00:00Z.
    Timestamptz(i64),
    /// A `uuid`, as its 16 bytes in network order.
    Uuid([u8; 16]),
}

impl Value {
    /// Return the value's scalar type, or `None` for an untyped NULL.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::{ScalarType, Value};
    ///
    /// assert_eq!(Value::from(7i32).scalar_type(), Some(ScalarType::Int4));
    /// assert_eq!(Value::Null.scalar_type(), None);
    /// ```
    #[must_use]
    pub const fn scalar_type(&self) -> Option<ScalarType> {
        Some(match self {
            Self::Null => return None,
            Self::Bool(_) => ScalarType::Bool,
            Self::Int2(_) => ScalarType::Int2,
            Self::Int4(_) => ScalarType::Int4,
            Self::Int8(_) => ScalarType::Int8,
            Self::Float8(_) => ScalarType::Float8,
            Self::Text(_) => ScalarType::Text,
            Self::Bytea(_) => ScalarType::Bytea,
            Self::Date(_) => ScalarType::Date,
            Self::Timestamptz(_) => ScalarType::Timestamptz,
            Self::Uuid(_) => ScalarType::Uuid,
        })
    }
}

/// Return a float's bit pattern, with every NaN mapped to one canonical NaN.
fn float_bits(v: f64) -> u64 {
    if v.is_nan() {
        f64::NAN.to_bits()
    } else {
        v.to_bits()
    }
}

impl PartialEq for Value {
    /// Compare structurally, floats by bit pattern.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int2(a), Self::Int2(b)) => a == b,
            (Self::Int4(a), Self::Int4(b)) => a == b,
            (Self::Int8(a), Self::Int8(b)) => a == b,
            (Self::Float8(a), Self::Float8(b)) => float_bits(*a) == float_bits(*b),
            (Self::Text(a), Self::Text(b)) => a == b,
            (Self::Bytea(a), Self::Bytea(b)) => a == b,
            (Self::Date(a), Self::Date(b)) => a == b,
            (Self::Timestamptz(a), Self::Timestamptz(b)) => a == b,
            (Self::Uuid(a), Self::Uuid(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl Hash for Value {
    /// Hash consistently with [`PartialEq`], floats by bit pattern.
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Null => {}
            Self::Bool(v) => v.hash(state),
            Self::Int2(v) => v.hash(state),
            Self::Int4(v) => v.hash(state),
            Self::Int8(v) => v.hash(state),
            Self::Float8(v) => float_bits(*v).hash(state),
            Self::Text(v) => v.hash(state),
            Self::Bytea(v) => v.hash(state),
            Self::Date(v) => v.hash(state),
            Self::Timestamptz(v) => v.hash(state),
            Self::Uuid(v) => v.hash(state),
        }
    }
}

/// Implement `From<$from> for Value` with the given variant.
macro_rules! value_from {
    ($from:ty, $variant:ident) => {
        impl From<$from> for Value {
            /// Wrap the Rust value as the matching literal.
            fn from(v: $from) -> Self {
                Self::$variant(v.into())
            }
        }
    };
}

value_from!(bool, Bool);
value_from!(i16, Int2);
value_from!(i32, Int4);
value_from!(i64, Int8);
value_from!(f64, Float8);
value_from!(&str, Text);
value_from!(String, Text);
value_from!(Vec<u8>, Bytea);
value_from!(&[u8], Bytea);
