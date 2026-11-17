//! Column values that can appear in an encoded key.

/// A single column value in a key tuple.
///
/// The variants cover the types in ADR-0003. Equality is structural, so
/// `Float8(NaN) != Float8(NaN)`; compare encoded bytes when you need
/// Postgres equality semantics (NaN equals NaN, `-0.0` equals `0.0`).
#[derive(Clone, Debug, PartialEq)]
pub enum Datum {
    /// SQL NULL.
    Null,
    /// `bool`.
    Bool(bool),
    /// `int2`.
    Int2(i16),
    /// `int4`.
    Int4(i32),
    /// `int8`.
    Int8(i64),
    /// `float8`.
    Float8(f64),
    /// `text`, compared with binary (C) collation.
    Text(String),
    /// `bytea`.
    Bytea(Vec<u8>),
}

impl Datum {
    /// Return `true` if this value is SQL NULL.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}
