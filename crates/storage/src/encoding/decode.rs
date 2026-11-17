//! Tuple decoder.

use super::encode::ordered_to_float_bits;
use super::tags::{
    BOOL, BYTEA, CANONICAL_NAN_BITS, ESCAPE, ESCAPED_ZERO, FLOAT8, INT2, INT4, INT8, NULL_FIRST,
    NULL_LAST, TERMINATOR, TEXT,
};
use super::{Datum, DecodeError, Direction, SortOrder};

/// Iterates over the columns of an encoded tuple without a schema.
///
/// Each item is the decoded value together with a [`SortOrder`] that would
/// re-encode it to the same bytes. For a value, the direction comes from the
/// tag and the NULL placement is the Postgres default for that direction; for
/// NULL, the placement comes from the tag and the direction is the one whose
/// default has that placement.
///
/// After an error the iterator is fused and yields `None`.
///
/// # Examples
///
/// ```
/// use interlaken_storage::encoding::{Datum, SortOrder, TupleDecoder, encode_tuple};
///
/// let bytes = encode_tuple(&[(Datum::Bool(true), SortOrder::DESC)])?;
/// let columns: Vec<_> = TupleDecoder::new(&bytes).collect::<Result<_, _>>()?;
/// assert_eq!(columns, vec![(Datum::Bool(true), SortOrder::DESC)]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct TupleDecoder<'a> {
    input: &'a [u8],
    pos: usize,
    failed: bool,
}

impl<'a> TupleDecoder<'a> {
    /// Construct a decoder over an encoded tuple.
    #[must_use]
    pub fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            failed: false,
        }
    }

    /// Return the byte offset of the next column.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.pos
    }

    /// Decode the column starting at the current position.
    fn decode_column(&mut self) -> Result<(Datum, SortOrder), DecodeError> {
        let start = self.pos;
        let raw_tag = self.input[start];
        self.pos += 1;

        match raw_tag {
            NULL_FIRST => return Ok((Datum::Null, SortOrder::DESC)),
            NULL_LAST => return Ok((Datum::Null, SortOrder::ASC)),
            _ => {}
        }

        let (tag, direction) = if is_value_tag(raw_tag) {
            (raw_tag, Direction::Asc)
        } else if is_value_tag(!raw_tag) {
            (!raw_tag, Direction::Desc)
        } else {
            return Err(DecodeError::UnknownTag {
                tag: raw_tag,
                offset: start,
            });
        };
        let mask = match direction {
            Direction::Asc => 0x00,
            Direction::Desc => 0xFF,
        };
        let order = match direction {
            Direction::Asc => SortOrder::ASC,
            Direction::Desc => SortOrder::DESC,
        };

        let datum = match tag {
            BOOL => match self.take_fixed::<1>(mask)? {
                [0] => Datum::Bool(false),
                [1] => Datum::Bool(true),
                _ => {
                    return Err(DecodeError::NonCanonical {
                        what: "bool",
                        offset: start,
                    });
                }
            },
            INT2 => {
                let raw = u16::from_be_bytes(self.take_fixed(mask)?) ^ 0x8000;
                Datum::Int2(raw.cast_signed())
            }
            INT4 => {
                let raw = u32::from_be_bytes(self.take_fixed(mask)?) ^ 0x8000_0000;
                Datum::Int4(raw.cast_signed())
            }
            INT8 => {
                let raw = u64::from_be_bytes(self.take_fixed(mask)?) ^ (1 << 63);
                Datum::Int8(raw.cast_signed())
            }
            FLOAT8 => {
                let bits = ordered_to_float_bits(u64::from_be_bytes(self.take_fixed(mask)?));
                let value = f64::from_bits(bits);
                if (value.is_nan() && bits != CANONICAL_NAN_BITS) || bits == (-0.0f64).to_bits() {
                    return Err(DecodeError::NonCanonical {
                        what: "float8",
                        offset: start,
                    });
                }
                Datum::Float8(value)
            }
            TEXT => {
                let bytes = self.take_escaped(mask)?;
                let text = String::from_utf8(bytes)
                    .map_err(|_| DecodeError::InvalidUtf8 { offset: start })?;
                Datum::Text(text)
            }
            BYTEA => Datum::Bytea(self.take_escaped(mask)?),
            _ => unreachable!("is_value_tag accepted {tag:#04x}"),
        };
        Ok((datum, order))
    }

    /// Read `N` body bytes, undoing DESC inversion with `mask`.
    fn take_fixed<const N: usize>(&mut self, mask: u8) -> Result<[u8; N], DecodeError> {
        let end = self.pos + N;
        let Some(slice) = self.input.get(self.pos..end) else {
            return Err(DecodeError::Truncated {
                offset: self.input.len(),
            });
        };
        let mut out = [0u8; N];
        for (dst, src) in out.iter_mut().zip(slice) {
            *dst = src ^ mask;
        }
        self.pos = end;
        Ok(out)
    }

    /// Read an escaped text/bytea body up to and including its terminator.
    fn take_escaped(&mut self, mask: u8) -> Result<Vec<u8>, DecodeError> {
        let mut out = Vec::new();
        loop {
            let byte = self.next_byte(mask)?;
            if byte != ESCAPE {
                out.push(byte);
                continue;
            }
            let escape_at = self.pos - 1;
            match self.next_byte(mask)? {
                ESCAPED_ZERO => out.push(ESCAPE),
                TERMINATOR => return Ok(out),
                _ => return Err(DecodeError::InvalidEscape { offset: escape_at }),
            }
        }
    }

    /// Read one body byte, undoing DESC inversion with `mask`.
    fn next_byte(&mut self, mask: u8) -> Result<u8, DecodeError> {
        let byte = *self.input.get(self.pos).ok_or(DecodeError::Truncated {
            offset: self.input.len(),
        })?;
        self.pos += 1;
        Ok(byte ^ mask)
    }
}

impl Iterator for TupleDecoder<'_> {
    type Item = Result<(Datum, SortOrder), DecodeError>;

    /// Decode the next column, or return `None` at the end of the tuple.
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.input.len() {
            return None;
        }
        let result = self.decode_column();
        self.failed = result.is_err();
        Some(result)
    }
}

impl std::iter::FusedIterator for TupleDecoder<'_> {}

/// Decode a tuple against the expected column sort orders.
///
/// Checks that the tuple has exactly `orders.len()` columns, that each value
/// column has the expected direction, and that each NULL has the expected
/// placement.
///
/// # Errors
///
/// Returns a [`DecodeError`] if the bytes are malformed, non-canonical, or
/// don't match `orders`.
///
/// # Examples
///
/// ```
/// use interlaken_storage::encoding::{Datum, SortOrder, decode_tuple, encode_tuple};
///
/// let columns = [(Datum::Int8(42), SortOrder::ASC), (Datum::Null, SortOrder::DESC)];
/// let bytes = encode_tuple(&columns)?;
/// let decoded = decode_tuple(&bytes, &[SortOrder::ASC, SortOrder::DESC])?;
/// assert_eq!(decoded, vec![Datum::Int8(42), Datum::Null]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn decode_tuple(input: &[u8], orders: &[SortOrder]) -> Result<Vec<Datum>, DecodeError> {
    let mut decoder = TupleDecoder::new(input);
    let mut out = Vec::with_capacity(orders.len());
    for (column, expected) in orders.iter().enumerate() {
        let Some(item) = decoder.next() else {
            return Err(DecodeError::ColumnCount {
                expected: orders.len(),
                found: column,
            });
        };
        let (datum, found) = item?;
        if datum.is_null() {
            if found.nulls != expected.nulls {
                return Err(DecodeError::NullOrderMismatch { column });
            }
        } else if found.direction != expected.direction {
            return Err(DecodeError::DirectionMismatch {
                column,
                expected: expected.direction,
                found: found.direction,
            });
        }
        out.push(datum);
    }
    if decoder.offset() < input.len() {
        return Err(DecodeError::ColumnCount {
            expected: orders.len(),
            found: orders.len() + 1,
        });
    }
    Ok(out)
}

/// Return `true` if `tag` is the ascending form of a value type tag.
fn is_value_tag(tag: u8) -> bool {
    matches!(tag, BOOL | INT2 | INT4 | INT8 | FLOAT8 | TEXT | BYTEA)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{NullOrder, encode_tuple};

    #[test]
    /// Assert unknown tags, bad escapes and truncation are reported.
    fn malformed_input() {
        assert_eq!(
            TupleDecoder::new(&[0x99]).next(),
            Some(Err(DecodeError::UnknownTag {
                tag: 0x99,
                offset: 0
            }))
        );
        assert_eq!(
            TupleDecoder::new(&[TEXT, b'a', 0x00, 0x02]).next(),
            Some(Err(DecodeError::InvalidEscape { offset: 2 }))
        );
        assert_eq!(
            TupleDecoder::new(&[INT4, 0x80]).next(),
            Some(Err(DecodeError::Truncated { offset: 2 }))
        );
        assert_eq!(
            TupleDecoder::new(&[TEXT, b'a']).next(),
            Some(Err(DecodeError::Truncated { offset: 2 }))
        );
    }

    #[test]
    /// Assert non-canonical encodings are rejected.
    fn non_canonical_rejected() {
        let neg_zero = ((-0.0f64).to_bits() ^ u64::MAX).to_be_bytes();
        let mut bytes = vec![FLOAT8];
        bytes.extend_from_slice(&neg_zero);
        assert!(matches!(
            TupleDecoder::new(&bytes).next(),
            Some(Err(DecodeError::NonCanonical { what: "float8", .. }))
        ));
        assert!(matches!(
            TupleDecoder::new(&[BOOL, 2]).next(),
            Some(Err(DecodeError::NonCanonical { what: "bool", .. }))
        ));
        assert!(matches!(
            TupleDecoder::new(&[TEXT, 0xC3, 0x00, 0x01]).next(),
            Some(Err(DecodeError::InvalidUtf8 { offset: 0 }))
        ));
    }

    #[test]
    /// Assert the decoder stops after the first error.
    fn fused_after_error() {
        let mut decoder = TupleDecoder::new(&[0x99, BOOL, 1]);
        assert!(matches!(decoder.next(), Some(Err(_))));
        assert_eq!(decoder.next(), None);
    }

    #[test]
    /// Assert schema checks catch count, direction and NULL placement mismatches.
    fn schema_mismatches() {
        let bytes = encode_tuple(&[
            (Datum::Int2(1), SortOrder::ASC),
            (Datum::Null, SortOrder::ASC),
        ])
        .unwrap();
        assert_eq!(
            decode_tuple(&bytes, &[SortOrder::ASC]),
            Err(DecodeError::ColumnCount {
                expected: 1,
                found: 2
            })
        );
        assert_eq!(
            decode_tuple(&bytes, &[SortOrder::ASC; 3]),
            Err(DecodeError::ColumnCount {
                expected: 3,
                found: 2
            })
        );
        assert_eq!(
            decode_tuple(&bytes, &[SortOrder::DESC, SortOrder::ASC]),
            Err(DecodeError::DirectionMismatch {
                column: 0,
                expected: Direction::Desc,
                found: Direction::Asc
            })
        );
        let nulls_first = SortOrder::new(Direction::Asc, NullOrder::First);
        assert_eq!(
            decode_tuple(&bytes, &[SortOrder::ASC, nulls_first]),
            Err(DecodeError::NullOrderMismatch { column: 1 })
        );
    }
}
