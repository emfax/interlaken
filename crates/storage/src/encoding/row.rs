//! The row (value) format (ADR-0003).
//!
//! Row values are not order-preserving. Layout, little-endian:
//!
//! | Part | Size |
//! | --- | --- |
//! | Schema version | `u32` |
//! | Column count `n` | `u16` |
//! | Null bitmap, bit `i` set when column `i` is NULL | `ceil(n / 8)` bytes, padding bits zero |
//! | Fixed-width columns, in column order, NULLs omitted | per type |
//! | Variable-length columns, in column order, NULLs omitted | `u32` length, then the bytes |
//!
//! An array value is variable-length. Its bytes are a `u32` element count
//! followed by the elements in the same layout as a row's columns: a NULL
//! bitmap, the fixed-width elements, then the variable-length ones.
//!
//! A row may hold fewer columns than the table's current definition: it was
//! written before `ALTER TABLE ... ADD COLUMN`, and the missing trailing
//! columns read as NULL. The decoder accepts only the canonical encoding
//! of the values it returns, so `encode(decode(b)) == b`.

use super::{ColumnType, Datum};

/// An error while encoding or decoding a row value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RowFormatError {
    /// The row has a different number of values than the table has columns.
    #[error("row has {found} values, table has {expected} columns")]
    ColumnCount {
        /// Columns in the table definition.
        expected: usize,
        /// Values supplied, or columns stored.
        found: usize,
    },
    /// A value does not match its column's type.
    #[error("column {column} expects {expected}")]
    TypeMismatch {
        /// Column position.
        column: usize,
        /// The column's type.
        expected: ColumnType,
    },
    /// A variable-length value or the column count exceeds the format's limits.
    #[error("row value too large")]
    TooLarge,
    /// The stored bytes end early, have trailing bytes, or hold an invalid value.
    #[error("corrupt row value: {0}")]
    Corrupt(&'static str),
}

/// Encode `values` as a row of a table whose columns have `types`.
///
/// # Errors
///
/// Returns [`RowFormatError::ColumnCount`] if the lengths differ,
/// [`RowFormatError::TypeMismatch`] if a non-NULL value has the wrong type,
/// or [`RowFormatError::TooLarge`] if a value is longer than `u32::MAX`
/// bytes or there are more than `u16::MAX` columns.
///
/// # Examples
///
/// ```
/// use interlaken_storage::encoding::{ColumnType, Datum, decode_row, encode_row};
///
/// let types = [ColumnType::Int8, ColumnType::Text];
/// let row = vec![Datum::Int8(1), Datum::Null];
/// let bytes = encode_row(3, &types, &row)?;
/// assert_eq!(decode_row(&bytes, &types)?, (3, row));
/// # Ok::<(), interlaken_storage::encoding::RowFormatError>(())
/// ```
pub fn encode_row(
    schema_version: u32,
    types: &[ColumnType],
    values: &[Datum],
) -> Result<Vec<u8>, RowFormatError> {
    if values.len() != types.len() {
        return Err(RowFormatError::ColumnCount {
            expected: types.len(),
            found: values.len(),
        });
    }
    for (i, (ty, value)) in types.iter().zip(values).enumerate() {
        if !ty.accepts(value) {
            return Err(RowFormatError::TypeMismatch {
                column: i,
                expected: *ty,
            });
        }
    }
    let count = u16::try_from(values.len()).map_err(|_| RowFormatError::TooLarge)?;
    let mut out = Vec::with_capacity(16 + values.len() * 8);
    out.extend_from_slice(&schema_version.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    write_values(&mut out, values)?;
    Ok(out)
}

/// Append the NULL bitmap, fixed-width values and variable-length values of
/// `values`, which have already been type-checked.
///
/// An array is a variable-length value whose body is a `u32` element count
/// followed by the elements in this same layout.
fn write_values(out: &mut Vec<u8>, values: &[Datum]) -> Result<(), RowFormatError> {
    let bitmap_start = out.len();
    out.resize(bitmap_start + values.len().div_ceil(8), 0);
    for (i, value) in values.iter().enumerate() {
        if value.is_null() {
            out[bitmap_start + i / 8] |= 1 << (i % 8);
        }
    }
    for value in values {
        match value {
            Datum::Bool(b) => out.push(u8::from(*b)),
            Datum::Int2(v) => out.extend_from_slice(&v.to_le_bytes()),
            Datum::Int4(v) => out.extend_from_slice(&v.to_le_bytes()),
            Datum::Int8(v) => out.extend_from_slice(&v.to_le_bytes()),
            Datum::Float8(v) => out.extend_from_slice(&v.to_bits().to_le_bytes()),
            Datum::Null | Datum::Text(_) | Datum::Bytea(_) | Datum::Array(_) => {}
        }
    }
    for value in values {
        let len_at = out.len();
        match value {
            Datum::Text(s) => write_bytes(out, s.as_bytes())?,
            Datum::Bytea(b) => write_bytes(out, b)?,
            Datum::Array(items) => {
                out.extend_from_slice(&[0; 4]);
                let count = u32::try_from(items.len()).map_err(|_| RowFormatError::TooLarge)?;
                out.extend_from_slice(&count.to_le_bytes());
                write_values(out, items)?;
                let len =
                    u32::try_from(out.len() - len_at - 4).map_err(|_| RowFormatError::TooLarge)?;
                out[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Append a `u32` length and then `bytes`.
fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), RowFormatError> {
    let len = u32::try_from(bytes.len()).map_err(|_| RowFormatError::TooLarge)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// Decode a row of a table whose current columns have `types`.
///
/// Returns the schema version the row was written under and one value per
/// entry in `types`; columns added after the row was written are NULL.
///
/// # Errors
///
/// Returns [`RowFormatError::ColumnCount`] if the row stores more columns
/// than `types` has, or [`RowFormatError::Corrupt`] if the bytes are not a
/// canonical row encoding.
pub fn decode_row(bytes: &[u8], types: &[ColumnType]) -> Result<(u32, Vec<Datum>), RowFormatError> {
    let mut reader = Reader { bytes, pos: 0 };
    let schema_version = u32::from_le_bytes(reader.array()?);
    let stored = usize::from(u16::from_le_bytes(reader.array()?));
    if stored > types.len() {
        return Err(RowFormatError::ColumnCount {
            expected: types.len(),
            found: stored,
        });
    }
    let mut values = read_values(&mut reader, &types[..stored])?;
    values.resize(types.len(), Datum::Null);
    if reader.pos != bytes.len() {
        return Err(RowFormatError::Corrupt("trailing bytes"));
    }
    Ok((schema_version, values))
}

/// Read one value per entry of `types`, in the layout [`write_values`] writes.
fn read_values(
    reader: &mut Reader<'_>,
    types: &[ColumnType],
) -> Result<Vec<Datum>, RowFormatError> {
    let n = types.len();
    let bitmap = reader.take(n.div_ceil(8))?;
    if !n.is_multiple_of(8) && bitmap[n / 8] >> (n % 8) != 0 {
        return Err(RowFormatError::Corrupt("nonzero null bitmap padding"));
    }
    let is_null = |i: usize| bitmap[i / 8] & (1 << (i % 8)) != 0;

    let mut values = vec![Datum::Null; n];
    for (i, ty) in types.iter().enumerate() {
        if is_null(i) {
            continue;
        }
        values[i] = match ty {
            ColumnType::Bool => match reader.array::<1>()?[0] {
                0 => Datum::Bool(false),
                1 => Datum::Bool(true),
                _ => return Err(RowFormatError::Corrupt("invalid bool")),
            },
            ColumnType::Int2 => Datum::Int2(i16::from_le_bytes(reader.array()?)),
            ColumnType::Int4 => Datum::Int4(i32::from_le_bytes(reader.array()?)),
            ColumnType::Int8 => Datum::Int8(i64::from_le_bytes(reader.array()?)),
            ColumnType::Float8 => {
                Datum::Float8(f64::from_bits(u64::from_le_bytes(reader.array()?)))
            }
            ColumnType::Text | ColumnType::Bytea | ColumnType::Array(_) => continue,
        };
    }
    for (i, ty) in types.iter().enumerate() {
        if is_null(i) || ty.fixed_width().is_some() {
            continue;
        }
        let len = u32::from_le_bytes(reader.array()?);
        let len = usize::try_from(len).map_err(|_| RowFormatError::TooLarge)?;
        let body = reader.take(len)?;
        values[i] = match ty {
            ColumnType::Text => {
                let text = std::str::from_utf8(body)
                    .map_err(|_| RowFormatError::Corrupt("text is not valid UTF-8"))?;
                Datum::Text(text.to_owned())
            }
            ColumnType::Array(element) => {
                let mut inner = Reader {
                    bytes: body,
                    pos: 0,
                };
                let count = u32::from_le_bytes(inner.array()?);
                let count = usize::try_from(count).map_err(|_| RowFormatError::TooLarge)?;
                // Every element takes at least one bitmap bit, so a count beyond
                // eight per byte is corrupt; checking first bounds the allocation.
                if count > body.len().saturating_mul(8) {
                    return Err(RowFormatError::Corrupt("array count exceeds its body"));
                }
                let items = read_values(&mut inner, &vec![element.column_type(); count])?;
                if inner.pos != body.len() {
                    return Err(RowFormatError::Corrupt("trailing bytes in an array"));
                }
                Datum::Array(items)
            }
            _ => Datum::Bytea(body.to_vec()),
        };
    }
    Ok(values)
}

/// A bounds-checked cursor over the row bytes.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Take the next `len` bytes.
    fn take(&mut self, len: usize) -> Result<&'a [u8], RowFormatError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.bytes.len())
            .ok_or(RowFormatError::Corrupt("truncated"))?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Take the next `N` bytes as an array.
    fn array<const N: usize>(&mut self) -> Result<[u8; N], RowFormatError> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One column of every type.
    const TYPES: [ColumnType; 7] = ColumnType::ALL;

    /// Return one non-NULL value per column of [`TYPES`].
    fn sample() -> Vec<Datum> {
        vec![
            Datum::Bool(true),
            Datum::Int2(-2),
            Datum::Int4(40_000),
            Datum::Int8(-1),
            Datum::Float8(-0.0),
            Datum::Text("héllo".into()),
            Datum::Bytea(vec![0, 1, 2]),
        ]
    }

    #[test]
    /// Assert every NULL pattern round-trips and re-encodes identically.
    fn round_trip_all_null_patterns() {
        for mask in 0u32..(1 << TYPES.len()) {
            let row: Vec<Datum> = sample()
                .into_iter()
                .enumerate()
                .map(|(i, d)| if mask & (1 << i) != 0 { Datum::Null } else { d })
                .collect();
            let bytes = encode_row(9, &TYPES, &row).unwrap();
            let (version, decoded) = decode_row(&bytes, &TYPES).unwrap();
            assert_eq!(version, 9);
            assert_eq!(format!("{decoded:?}"), format!("{row:?}"));
            assert_eq!(encode_row(9, &TYPES, &decoded).unwrap(), bytes);
        }
    }

    #[test]
    /// Assert arrays of every element type round-trip, with NULL elements and when empty.
    fn arrays_round_trip() {
        use super::super::ElementType;
        for (scalar, value) in TYPES.iter().zip(sample()) {
            let ty = ColumnType::Array(ElementType::of(*scalar).unwrap());
            for items in [
                vec![],
                vec![value.clone()],
                vec![Datum::Null, value.clone(), Datum::Null],
                vec![value.clone(); 9],
            ] {
                let row = vec![Datum::Int8(1), Datum::Array(items), Datum::Text("t".into())];
                let types = [ColumnType::Int8, ty, ColumnType::Text];
                let bytes = encode_row(2, &types, &row).unwrap();
                let (_, decoded) = decode_row(&bytes, &types).unwrap();
                assert_eq!(format!("{decoded:?}"), format!("{row:?}"));
                assert_eq!(encode_row(2, &types, &decoded).unwrap(), bytes);
                for len in 0..bytes.len() {
                    assert!(decode_row(&bytes[..len], &types).is_err(), "prefix {len}");
                }
            }
        }
        let ints = ColumnType::Array(ElementType::Int4);
        assert!(matches!(
            encode_row(1, &[ints], &[Datum::Array(vec![Datum::Int8(1)])]),
            Err(RowFormatError::TypeMismatch { column: 0, .. })
        ));
        assert!(matches!(
            encode_row(1, &[ColumnType::Int4], &[Datum::Array(vec![])]),
            Err(RowFormatError::TypeMismatch { column: 0, .. })
        ));
    }

    #[test]
    /// Assert rows written before a column was added read it as NULL.
    fn added_columns_read_as_null() {
        let bytes = encode_row(1, &TYPES[..2], &sample()[..2]).unwrap();
        let (_, decoded) = decode_row(&bytes, &TYPES).unwrap();
        assert_eq!(decoded[..2], sample()[..2]);
        assert!(decoded[2..].iter().all(Datum::is_null));
        assert!(matches!(
            decode_row(&encode_row(1, &TYPES, &sample()).unwrap(), &TYPES[..3]),
            Err(RowFormatError::ColumnCount { .. })
        ));
    }

    #[test]
    /// Assert type mismatches and corrupt input are rejected.
    fn rejects_invalid_input() {
        assert!(matches!(
            encode_row(1, &[ColumnType::Int4], &[Datum::Int8(1)]),
            Err(RowFormatError::TypeMismatch { column: 0, .. })
        ));
        let bytes = encode_row(1, &TYPES, &sample()).unwrap();
        for len in 0..bytes.len() {
            assert!(decode_row(&bytes[..len], &TYPES).is_err(), "prefix {len}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_row(&trailing, &TYPES).is_err());
        let mut padding = encode_row(1, &[ColumnType::Bool], &[Datum::Null]).unwrap();
        padding[6] |= 0b10;
        assert!(decode_row(&padding, &[ColumnType::Bool, ColumnType::Bool]).is_err());
    }
}
