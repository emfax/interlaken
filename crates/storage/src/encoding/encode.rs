//! Tuple encoder.

use super::tags::{
    BOOL, BYTEA, CANONICAL_NAN_BITS, ESCAPE, ESCAPED_ZERO, FLOAT8, INT2, INT4, INT8, NULL_FIRST,
    NULL_LAST, TERMINATOR, TEXT,
};
use super::{Datum, Direction, EncodeError, NullOrder, SortOrder, VERSION_SUFFIX_LEN};

/// Default cap on an encoded key tuple, per ADR-0003.
pub const DEFAULT_MAX_KEY_LEN: usize = 8 * 1024;

/// Hard upper bound on an encoded key tuple.
///
/// fjall limits keys to `u16::MAX` bytes, and a stored key also carries the
/// version suffix.
pub const MAX_TUPLE_LEN: usize = u16::MAX as usize - VERSION_SUFFIX_LEN;

/// Mask for the sign bit of a 64-bit value.
const SIGN_BIT: u64 = 1 << 63;

/// Builds an encoded key tuple one column at a time.
///
/// # Examples
///
/// ```
/// use interlaken_storage::encoding::{Datum, KeyBuilder, SortOrder};
///
/// let mut key = KeyBuilder::new();
/// key.push(&Datum::Int8(7), SortOrder::ASC)
///     .push(&Datum::Text("alice".into()), SortOrder::DESC);
/// let bytes = key.finish()?;
/// assert!(!bytes.is_empty());
/// # Ok::<(), interlaken_storage::encoding::EncodeError>(())
/// ```
#[derive(Clone, Debug)]
pub struct KeyBuilder {
    buf: Vec<u8>,
    max_len: usize,
    /// An array was pushed; arrays have no key encoding.
    unkeyable: bool,
}

impl KeyBuilder {
    /// Construct an empty builder with the [`DEFAULT_MAX_KEY_LEN`] limit.
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_len(DEFAULT_MAX_KEY_LEN)
    }

    /// Construct an empty builder that rejects tuples longer than `max_len` bytes.
    ///
    /// # Panics
    ///
    /// Panics if `max_len` exceeds [`MAX_TUPLE_LEN`], since fjall could not
    /// store such a key.
    #[must_use]
    pub fn with_max_len(max_len: usize) -> Self {
        assert!(
            max_len <= MAX_TUPLE_LEN,
            "max_len {max_len} exceeds the fjall limit of {MAX_TUPLE_LEN}"
        );
        Self {
            buf: Vec::new(),
            max_len,
            unkeyable: false,
        }
    }

    /// Append one column, encoded under the given sort order.
    pub fn push(&mut self, datum: &Datum, order: SortOrder) -> &mut Self {
        let start = self.buf.len();
        match datum {
            Datum::Null => {
                // NULL tags encode the final placement and are never inverted.
                self.buf.push(match order.nulls {
                    NullOrder::First => NULL_FIRST,
                    NullOrder::Last => NULL_LAST,
                });
                return self;
            }
            Datum::Bool(v) => {
                self.buf.push(BOOL);
                self.buf.push(u8::from(*v));
            }
            Datum::Int2(v) => {
                self.buf.push(INT2);
                self.buf
                    .extend_from_slice(&(v.cast_unsigned() ^ 0x8000).to_be_bytes());
            }
            Datum::Int4(v) => {
                self.buf.push(INT4);
                self.buf
                    .extend_from_slice(&(v.cast_unsigned() ^ 0x8000_0000).to_be_bytes());
            }
            Datum::Int8(v) => {
                self.buf.push(INT8);
                self.buf
                    .extend_from_slice(&(v.cast_unsigned() ^ SIGN_BIT).to_be_bytes());
            }
            Datum::Float8(v) => {
                self.buf.push(FLOAT8);
                self.buf
                    .extend_from_slice(&float_to_ordered(*v).to_be_bytes());
            }
            Datum::Text(s) => {
                self.buf.push(TEXT);
                self.push_escaped(s.as_bytes());
            }
            Datum::Bytea(b) => {
                self.buf.push(BYTEA);
                self.push_escaped(b);
            }
            Datum::Array(_) => {
                // Arrays have no key encoding; `finish` reports it.
                self.unkeyable = true;
                return self;
            }
        }
        if order.direction == Direction::Desc {
            for byte in &mut self.buf[start..] {
                *byte = !*byte;
            }
        }
        self
    }

    /// Return the number of bytes encoded so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Return `true` if no columns have been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Consume the builder and return the encoded tuple.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::KeyTooLong`] if the tuple exceeds the builder's limit,
    /// or [`EncodeError::Unkeyable`] if an array was pushed.
    pub fn finish(self) -> Result<Vec<u8>, EncodeError> {
        if self.unkeyable {
            return Err(EncodeError::Unkeyable);
        }
        if self.buf.len() > self.max_len {
            return Err(EncodeError::KeyTooLong {
                len: self.buf.len(),
                max: self.max_len,
            });
        }
        Ok(self.buf)
    }

    /// Append a text/bytea body: escape each `0x00`, then write the terminator.
    fn push_escaped(&mut self, bytes: &[u8]) {
        self.buf.reserve(bytes.len() + 2);
        for &b in bytes {
            if b == ESCAPE {
                self.buf.extend_from_slice(&[ESCAPE, ESCAPED_ZERO]);
            } else {
                self.buf.push(b);
            }
        }
        self.buf.extend_from_slice(&[ESCAPE, TERMINATOR]);
    }
}

impl Default for KeyBuilder {
    /// Construct an empty builder with the default limit.
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a whole tuple with the [`DEFAULT_MAX_KEY_LEN`] limit.
///
/// # Errors
///
/// Returns [`EncodeError::KeyTooLong`] if the encoded tuple exceeds the limit.
///
/// # Examples
///
/// ```
/// use interlaken_storage::encoding::{Datum, SortOrder, encode_tuple};
///
/// let a = encode_tuple(&[(Datum::Int4(-1), SortOrder::ASC)])?;
/// let b = encode_tuple(&[(Datum::Int4(1), SortOrder::ASC)])?;
/// assert!(a < b);
/// # Ok::<(), interlaken_storage::encoding::EncodeError>(())
/// ```
pub fn encode_tuple(columns: &[(Datum, SortOrder)]) -> Result<Vec<u8>, EncodeError> {
    let mut builder = KeyBuilder::new();
    for (datum, order) in columns {
        builder.push(datum, *order);
    }
    builder.finish()
}

/// Encode a float as a `u64` whose unsigned order matches Postgres float order.
///
/// NaN is canonicalised and sorts above `+inf`; `-0.0` is encoded as `0.0`.
pub(crate) fn float_to_ordered(v: f64) -> u64 {
    let bits = if v.is_nan() {
        CANONICAL_NAN_BITS
    } else if v == 0.0 {
        0
    } else {
        v.to_bits()
    };
    if bits & SIGN_BIT == 0 {
        bits | SIGN_BIT
    } else {
        !bits
    }
}

/// Invert [`float_to_ordered`], returning raw IEEE 754 bits.
pub(crate) fn ordered_to_float_bits(ordered: u64) -> u64 {
    if ordered & SIGN_BIT == 0 {
        !ordered
    } else {
        ordered & !SIGN_BIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a single ascending column.
    fn asc(datum: Datum) -> Vec<u8> {
        encode_tuple(&[(datum, SortOrder::ASC)]).unwrap()
    }

    #[test]
    /// Assert integers encode as big-endian with the sign bit flipped.
    fn integers_flip_sign_bit() {
        assert_eq!(asc(Datum::Int2(0)), [INT2, 0x80, 0x00]);
        assert_eq!(asc(Datum::Int2(-1)), [INT2, 0x7F, 0xFF]);
        assert_eq!(asc(Datum::Int4(i32::MIN)), [INT4, 0, 0, 0, 0]);
        assert_eq!(
            asc(Datum::Int8(i64::MAX)),
            [INT8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
    }

    #[test]
    /// Assert text escapes zero bytes and ends with the terminator.
    fn text_escapes_zero() {
        assert_eq!(
            asc(Datum::Text("a\0b".into())),
            [TEXT, b'a', 0x00, 0xFF, b'b', 0x00, 0x01]
        );
        assert_eq!(asc(Datum::Bytea(vec![])), [BYTEA, 0x00, 0x01]);
    }

    #[test]
    /// Assert NULL tags are not inverted for DESC columns.
    fn null_placement_is_absolute() {
        let first = SortOrder::new(Direction::Desc, NullOrder::First);
        let last = SortOrder::new(Direction::Desc, NullOrder::Last);
        assert_eq!(encode_tuple(&[(Datum::Null, first)]).unwrap(), [NULL_FIRST]);
        assert_eq!(encode_tuple(&[(Datum::Null, last)]).unwrap(), [NULL_LAST]);
    }

    #[test]
    /// Assert DESC inverts the tag, body and terminator.
    fn desc_inverts_every_byte() {
        let asc_bytes = asc(Datum::Text("x".into()));
        let desc_bytes = encode_tuple(&[(Datum::Text("x".into()), SortOrder::DESC)]).unwrap();
        let inverted: Vec<u8> = asc_bytes.iter().map(|b| !b).collect();
        assert_eq!(desc_bytes, inverted);
    }

    #[test]
    /// Assert float special values sort as Postgres orders them.
    fn float_special_values() {
        let ordered = [
            f64::NEG_INFINITY,
            -1.5,
            -0.0,
            1e-300,
            2.0,
            f64::INFINITY,
            f64::NAN,
        ];
        let encoded: Vec<_> = ordered.iter().map(|v| float_to_ordered(*v)).collect();
        assert!(encoded.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(float_to_ordered(-0.0), float_to_ordered(0.0));
        assert_eq!(float_to_ordered(-f64::NAN), float_to_ordered(f64::NAN));
        assert_eq!(
            ordered_to_float_bits(float_to_ordered(-1.5)),
            (-1.5f64).to_bits()
        );
    }

    #[test]
    /// Assert tuples over the limit are rejected.
    fn key_too_long() {
        let mut builder = KeyBuilder::with_max_len(4);
        builder.push(&Datum::Int8(1), SortOrder::ASC);
        assert_eq!(
            builder.finish(),
            Err(EncodeError::KeyTooLong { len: 9, max: 4 })
        );
        assert_eq!(
            encode_tuple(&[(Datum::Array(vec![]), SortOrder::ASC)]),
            Err(EncodeError::Unkeyable)
        );
    }

    #[test]
    #[should_panic(expected = "exceeds the fjall limit")]
    /// Assert a limit beyond what fjall can store panics.
    fn max_len_above_fjall_limit_panics() {
        let _ = KeyBuilder::with_max_len(MAX_TUPLE_LEN + 1);
    }
}
