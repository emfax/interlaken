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

    /// Return the next logical timestamp, or `None` if the space is exhausted.
    ///
    /// Only the timestamp source (the commit path) may call this; everywhere
    /// else timestamps are opaque and only compared.
    pub(crate) const fn successor(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }
}

/// A timestamp's text form didn't parse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a commit timestamp: {0:?}")]
pub struct ParseTimestampError(pub String);

impl std::str::FromStr for Timestamp {
    type Err = ParseTimestampError;

    /// Parse a timestamp from the text form [`Display`](fmt::Display) writes.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseTimestampError(text.to_owned()));
        }
        text.parse()
            .map(Self)
            .map_err(|_| ParseTimestampError(text.to_owned()))
    }
}

impl fmt::Display for Timestamp {
    /// Render the timestamp as its raw decimal value, the stable text form
    /// clients carry between members (ADR-0040).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
