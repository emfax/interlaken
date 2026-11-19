//! Encoding and decoding errors.

use super::Direction;

/// An error while encoding a key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EncodeError {
    /// The encoded tuple exceeds the configured maximum key length.
    #[error("encoded key is {len} bytes, exceeding the {max}-byte limit")]
    KeyTooLong {
        /// Length of the encoded tuple.
        len: usize,
        /// Configured limit.
        max: usize,
    },
    /// The tuple contains an array, which has no key encoding.
    #[error("arrays can't be part of a key")]
    Unkeyable,
}

/// An error while decoding a key.
///
/// Offsets are byte positions in the input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The input ended inside a column.
    #[error("key truncated at byte {offset}")]
    Truncated {
        /// Offset at which more bytes were expected.
        offset: usize,
    },

    /// A column starts with a byte that is not a known type tag.
    #[error("unknown type tag {tag:#04x} at byte {offset}")]
    UnknownTag {
        /// The unrecognised byte.
        tag: u8,
        /// Its offset.
        offset: usize,
    },

    /// A text or bytea body contains an escape byte followed by something
    /// other than an escaped zero or a terminator.
    #[error("invalid escape sequence at byte {offset}")]
    InvalidEscape {
        /// Offset of the escape byte.
        offset: usize,
    },

    /// The bytes decode to a value but are not the canonical encoding of it.
    #[error("non-canonical {what} at byte {offset}")]
    NonCanonical {
        /// What was non-canonical.
        what: &'static str,
        /// Offset of the column's tag.
        offset: usize,
    },

    /// A text column is not valid UTF-8.
    #[error("text column at byte {offset} is not valid UTF-8")]
    InvalidUtf8 {
        /// Offset of the column's tag.
        offset: usize,
    },

    /// The tuple has a different number of columns than expected.
    #[error("expected {expected} columns, found {found}")]
    ColumnCount {
        /// Columns the caller expected.
        expected: usize,
        /// Columns present (or at least present, if there were more).
        found: usize,
    },

    /// A value column was encoded with a different direction than expected.
    #[error("column {column} is encoded {found:?}, expected {expected:?}")]
    DirectionMismatch {
        /// Zero-based column index.
        column: usize,
        /// Direction the caller expected.
        expected: Direction,
        /// Direction found in the encoding.
        found: Direction,
    },

    /// A NULL column was encoded with a different NULL placement than expected.
    #[error("NULL in column {column} has the wrong placement")]
    NullOrderMismatch {
        /// Zero-based column index.
        column: usize,
    },

    /// A versioned key is too short to hold a version suffix.
    #[error("versioned key is {len} bytes, too short for a version suffix")]
    MissingVersion {
        /// Length of the key.
        len: usize,
    },
}
