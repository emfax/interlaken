//! Runtime values.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use crate::value::Value;

/// A runtime value: a scalar, an array or a record, or NULL.
#[derive(Clone, Debug)]
pub enum Datum {
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
    /// A `timestamptz`: microseconds since the Unix epoch.
    Timestamptz(i64),
    /// A `uuid`.
    Uuid([u8; 16]),
    /// A `jsonb` value, held as its text.
    Jsonb(String),
    /// An array.
    Array(Vec<Datum>),
    /// A record's field values, in field order.
    Record(Vec<Datum>),
}

impl From<&Value> for Datum {
    /// Convert a literal.
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(v) => Self::Bool(*v),
            Value::Int2(v) => Self::Int2(*v),
            Value::Int4(v) => Self::Int4(*v),
            Value::Int8(v) => Self::Int8(*v),
            Value::Float8(v) => Self::Float8(*v),
            Value::Text(v) => Self::Text(v.clone()),
            Value::Bytea(v) => Self::Bytea(v.clone()),
            Value::Date(v) => Self::Date(*v),
            Value::Timestamptz(v) => Self::Timestamptz(*v),
            Value::Uuid(v) => Self::Uuid(*v),
        }
    }
}

/// Compare floats as Postgres does: NaN equals NaN and sorts above everything; -0 equals 0.
fn compare_floats(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).expect("neither is NaN"),
    }
}

impl Datum {
    /// Return `true` for NULL.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Compare two non-NULL values of the same type; `None` if either is
    /// NULL or the types differ.
    ///
    /// Text and bytes compare bytewise (C collation); arrays and records
    /// compare element by element.
    #[must_use]
    pub fn compare(&self, other: &Self) -> Option<Ordering> {
        Some(match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => a.cmp(b),
            (Self::Int2(a), Self::Int2(b)) => a.cmp(b),
            (Self::Int4(a), Self::Int4(b)) => a.cmp(b),
            (Self::Int8(a), Self::Int8(b)) => a.cmp(b),
            (Self::Float8(a), Self::Float8(b)) => compare_floats(*a, *b),
            (Self::Text(a), Self::Text(b)) | (Self::Jsonb(a), Self::Jsonb(b)) => {
                a.as_bytes().cmp(b.as_bytes())
            }
            (Self::Bytea(a), Self::Bytea(b)) => a.cmp(b),
            (Self::Date(a), Self::Date(b)) => a.cmp(b),
            (Self::Timestamptz(a), Self::Timestamptz(b)) => a.cmp(b),
            (Self::Uuid(a), Self::Uuid(b)) => a.cmp(b),
            (Self::Array(a), Self::Array(b)) | (Self::Record(a), Self::Record(b)) => {
                for (x, y) in a.iter().zip(b) {
                    match (x.is_null(), y.is_null()) {
                        (true, true) => continue,
                        (true, false) => return Some(Ordering::Greater),
                        (false, true) => return Some(Ordering::Less),
                        _ => match x.compare(y)? {
                            Ordering::Equal => continue,
                            other => return Some(other),
                        },
                    }
                }
                a.len().cmp(&b.len())
            }
            _ => return None,
        })
    }

    /// Return `true` if the two values are not distinct: both NULL, or equal.
    #[must_use]
    pub fn not_distinct(&self, other: &Self) -> bool {
        match (self.is_null(), other.is_null()) {
            (true, true) => true,
            (false, false) => self.compare(other) == Some(Ordering::Equal),
            _ => false,
        }
    }

    /// Feed a hash consistent with [`Datum::not_distinct`].
    fn hash_into<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Null => {}
            Self::Bool(v) => v.hash(state),
            Self::Int2(v) => v.hash(state),
            Self::Int4(v) => v.hash(state),
            Self::Int8(v) => v.hash(state),
            Self::Float8(v) => {
                let canonical = if v.is_nan() {
                    f64::NAN.to_bits()
                } else if *v == 0.0 {
                    0
                } else {
                    v.to_bits()
                };
                canonical.hash(state);
            }
            Self::Text(v) | Self::Jsonb(v) => v.hash(state),
            Self::Bytea(v) => v.hash(state),
            Self::Date(v) => v.hash(state),
            Self::Timestamptz(v) => v.hash(state),
            Self::Uuid(v) => v.hash(state),
            Self::Array(items) | Self::Record(items) => {
                items.len().hash(state);
                items.iter().for_each(|d| d.hash_into(state));
            }
        }
    }
}

/// A row used as a grouping or duplicate-elimination key: rows are equal
/// when every column is not distinct (NULLs equal, NaN equal to NaN).
#[derive(Clone, Debug)]
pub struct RowKey(pub Vec<Datum>);

impl PartialEq for RowKey {
    /// Compare column by column with `IS NOT DISTINCT FROM`.
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len() && self.0.iter().zip(&other.0).all(|(a, b)| a.not_distinct(b))
    }
}

impl Eq for RowKey {}

impl Hash for RowKey {
    /// Hash consistently with equality.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.len().hash(state);
        self.0.iter().for_each(|d| d.hash_into(state));
    }
}
