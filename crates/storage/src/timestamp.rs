//! The opaque commit timestamp.

use std::fmt;

/// A commit timestamp: an opaque, totally ordered 64-bit value.
///
/// Per ADR-0002, code outside the timestamp source only compares timestamps
/// and never does arithmetic on them. That keeps the key format unchanged if
/// the logical counter used by the MVP is later replaced by a Raft-derived
/// timestamp or a hybrid logical clock packed into the same 64 bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    /// The smallest timestamp, before any commit.
    pub const ZERO: Self = Self(0);

    /// The largest representable timestamp.
    pub const MAX: Self = Self(u64::MAX);

    /// Construct a timestamp from its raw 64-bit representation.
    ///
    /// Intended for the timestamp source and for decoding persisted values.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_storage::Timestamp;
    ///
    /// assert!(Timestamp::from_raw(2) > Timestamp::from_raw(1));
    /// ```
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the raw 64-bit representation, for encoding into keys and metadata.
    #[must_use]
    pub const fn into_raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Timestamp {
    /// Render the timestamp as its raw decimal value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
