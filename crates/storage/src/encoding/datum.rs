//! Column values.

/// A single column value.
///
/// The scalar variants cover the types in ADR-0003 and can appear in keys;
/// arrays appear only in row values. Equality is structural, so
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
    /// An array of scalar values (or NULLs); never part of a key.
    Array(Vec<Datum>),
}

impl Datum {
    /// Return `true` if this value is SQL NULL.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}
