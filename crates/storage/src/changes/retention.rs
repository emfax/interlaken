//! How long the change log keeps changes.

use std::time::Duration;

/// How much of the change log a database keeps (ADR-0040).
///
/// A change feed can resume from any commit after the log's floor: the
/// oldest point it can replay from without a gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeRetention {
    /// Keep no change log: change feeds are unavailable.
    Off,
    /// Keep the most recent changes in memory, up to this many bytes of
    /// keys and values. The log starts empty at every open.
    Memory {
        /// Most bytes to keep.
        bytes: u64,
    },
    /// Also write each transaction's changes durably, in the batch that
    /// writes its commit, and keep them for `age` and at most `bytes`. The
    /// log survives restarts; it roughly doubles the bytes written per
    /// commit.
    Durable {
        /// How long to keep changes, by commit time.
        age: Duration,
        /// Most bytes of keys and values to keep.
        bytes: u64,
    },
}

impl ChangeRetention {
    /// The default: 64 MiB in memory.
    pub const DEFAULT: Self = Self::Memory {
        bytes: 64 * 1024 * 1024,
    };

    /// Return the most bytes kept in memory.
    pub(crate) fn memory_bytes(self) -> u64 {
        match self {
            Self::Off => 0,
            Self::Memory { bytes } => bytes,
            // Memory caches recent durable changes, for feeds that keep up.
            Self::Durable { bytes, .. } => bytes.min(Self::DEFAULT.memory_bytes()),
        }
    }
}

impl Default for ChangeRetention {
    /// Return [`ChangeRetention::DEFAULT`].
    fn default() -> Self {
        Self::DEFAULT
    }
}
