//! The transaction's read set: point keys and key ranges (ADR-0004).
//!
//! Recorded now and checked by commit validation in milestone M3.

use std::collections::BTreeSet;

use crate::scan::{Key, KeyRange};

/// Keys and ranges a transaction has read.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReadSet {
    keys: BTreeSet<Key>,
    ranges: Vec<KeyRange>,
}

impl ReadSet {
    /// Register a point read.
    pub(crate) fn record_key(&mut self, key: Key) {
        self.keys.insert(key);
    }

    /// Register a range read.
    pub(crate) fn record_range(&mut self, range: KeyRange) {
        self.ranges.push(range);
    }

    /// Return the registered point keys.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "read by commit validation in M3")
    )]
    pub(crate) fn keys(&self) -> &BTreeSet<Key> {
        &self.keys
    }

    /// Return the registered ranges, in registration order.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "read by commit validation in M3")
    )]
    pub(crate) fn ranges(&self) -> &[KeyRange] {
        &self.ranges
    }
}
