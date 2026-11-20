//! Type tag bytes.
//!
//! Value tags are listed in their ascending form; a DESC column stores the
//! inverted tag. The two forms occupy disjoint ranges (`0x10..=0x50` and
//! `0xAF..=0xEF`), and both lie strictly between the two NULL tags, so NULL
//! placement holds regardless of direction.

/// NULL that sorts before all values.
pub(crate) const NULL_FIRST: u8 = 0x01;
/// NULL that sorts after all values.
pub(crate) const NULL_LAST: u8 = 0xFE;

/// `bool`, ascending form.
pub(crate) const BOOL: u8 = 0x10;
/// `int2`, ascending form.
pub(crate) const INT2: u8 = 0x20;
/// `int4`, ascending form.
pub(crate) const INT4: u8 = 0x21;
/// `int8`, ascending form.
pub(crate) const INT8: u8 = 0x22;
/// `date`, ascending form.
pub(crate) const DATE: u8 = 0x23;
/// `timestamptz`, ascending form.
pub(crate) const TIMESTAMPTZ: u8 = 0x24;
/// `float8`, ascending form.
pub(crate) const FLOAT8: u8 = 0x30;
/// `text`, ascending form.
pub(crate) const TEXT: u8 = 0x40;
/// `bytea`, ascending form.
pub(crate) const BYTEA: u8 = 0x41;
/// `uuid`, ascending form: sixteen bytes, no escaping.
pub(crate) const UUID: u8 = 0x50;

/// Byte that introduces an escape or terminator inside a text/bytea body.
pub(crate) const ESCAPE: u8 = 0x00;
/// Follows [`ESCAPE`] to encode a literal `0x00`.
pub(crate) const ESCAPED_ZERO: u8 = 0xFF;
/// Follows [`ESCAPE`] to end a text/bytea body.
pub(crate) const TERMINATOR: u8 = 0x01;

/// The only `float8` NaN bit pattern the encoding produces.
pub(crate) const CANONICAL_NAN_BITS: u64 = 0x7FF8_0000_0000_0000;
