//! Counters the sequencer updates as it works, for throughput measurement.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Cumulative commit pipeline counters since the database was opened.
///
/// Read them twice and subtract to measure an interval. The sequencer is
/// one thread, so `validation + write` over wall-clock time is how busy it
/// was: near 100% means it, not the callers, limits commit throughput.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitStats {
    /// Groups written to fjall (one batch, and one sync under `Sync`, each).
    pub groups: u64,
    /// Transactions committed.
    pub committed: u64,
    /// Transactions rejected by validation.
    pub conflicts: u64,
    /// Sequencer time spent validating, trimming history and acknowledging.
    pub validation: Duration,
    /// Sequencer time spent writing (and syncing) group batches.
    pub write: Duration,
}

impl CommitStats {
    /// Return the counters accumulated between `earlier` and `self`.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_storage::CommitStats;
    ///
    /// let before = CommitStats { committed: 3, ..CommitStats::default() };
    /// let after = CommitStats { committed: 10, ..CommitStats::default() };
    /// assert_eq!(after.since(&before).committed, 7);
    /// ```
    #[must_use]
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            groups: self.groups - earlier.groups,
            committed: self.committed - earlier.committed,
            conflicts: self.conflicts - earlier.conflicts,
            validation: self.validation.saturating_sub(earlier.validation),
            write: self.write.saturating_sub(earlier.write),
        }
    }

    /// Return the mean number of transactions per written group.
    #[must_use]
    pub fn mean_group_size(&self) -> f64 {
        if self.groups == 0 {
            0.0
        } else {
            self.committed as f64 / self.groups as f64
        }
    }
}

/// The live counters behind [`CommitStats`].
#[derive(Debug, Default)]
pub(crate) struct SequencerStats {
    groups: AtomicU64,
    committed: AtomicU64,
    conflicts: AtomicU64,
    validation_nanos: AtomicU64,
    write_nanos: AtomicU64,
}

impl SequencerStats {
    /// Record one round of the sequencer loop.
    pub(crate) fn record_round(
        &self,
        committed: u64,
        conflicts: u64,
        validation: Duration,
        write: Duration,
    ) {
        if committed > 0 {
            self.groups.fetch_add(1, Ordering::Relaxed);
        }
        self.committed.fetch_add(committed, Ordering::Relaxed);
        self.conflicts.fetch_add(conflicts, Ordering::Relaxed);
        self.validation_nanos
            .fetch_add(nanos(validation), Ordering::Relaxed);
        self.write_nanos.fetch_add(nanos(write), Ordering::Relaxed);
    }

    /// Return a snapshot of the counters.
    pub(crate) fn snapshot(&self) -> CommitStats {
        CommitStats {
            groups: self.groups.load(Ordering::Relaxed),
            committed: self.committed.load(Ordering::Relaxed),
            conflicts: self.conflicts.load(Ordering::Relaxed),
            validation: Duration::from_nanos(self.validation_nanos.load(Ordering::Relaxed)),
            write: Duration::from_nanos(self.write_nanos.load(Ordering::Relaxed)),
        }
    }
}

/// Convert a duration to whole nanoseconds, saturating.
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}
