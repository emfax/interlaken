//! Key encoding: order-preserving tuples (ADR-0003) and versioned keys (ADR-0002).
//!
//! A tuple is a sequence of columns. Each column is a type tag followed by a
//! type-specific body, and the encoding is designed so that comparing two
//! encoded tuples byte by byte gives the same result as comparing their values
//! column by column, honouring each column's [`SortOrder`]:
//!
//! | Type | Body |
//! | --- | --- |
//! | NULL | No body. The tag alone places it first or last ([`NullOrder`]) |
//! | bool | `0x00` / `0x01` |
//! | int2 / int4 / int8 | Big-endian, sign bit flipped |
//! | float8 | Negative: all bits inverted; otherwise sign bit flipped. One canonical NaN, above all numbers; `-0.0` encodes as `0.0` |
//! | text / bytea | Raw bytes, `0x00` escaped as `0x00 0xFF`, terminated by `0x00 0x01` |
//!
//! A DESC column has every byte of its tag and body inverted. NULL tags are
//! never inverted: they already encode the final placement.
//!
//! Encodings are canonical: values that compare equal encode to identical
//! bytes, and the decoder rejects any byte string the encoder would not
//! produce. So `encode(decode(bytes)) == bytes` whenever decoding succeeds.
//!
//! # Tuple arity
//!
//! Tuples are self-delimiting, but a tuple is a byte prefix of any longer
//! tuple that extends it. All keys under one `(table_id, index_id)` prefix
//! must therefore have the same columns, so that the version suffix of one
//! key never interleaves with the columns of another (see [`versioned_key`]).

mod column_type;
mod datum;
mod decode;
mod encode;
mod error;
mod order;
mod row;
mod tags;
mod versioned;

pub use column_type::{ColumnType, ElementType};
pub use datum::Datum;
pub use decode::{TupleDecoder, decode_tuple};
pub use encode::{DEFAULT_MAX_KEY_LEN, KeyBuilder, MAX_TUPLE_LEN, encode_tuple};
pub use error::{DecodeError, EncodeError};
pub use order::{Direction, NullOrder, SortOrder};
pub use row::{RowFormatError, decode_row, encode_row};
pub use versioned::{VERSION_SUFFIX_LEN, split_versioned_key, versioned_key};
