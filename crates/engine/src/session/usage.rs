//! What a statement cost (ADR-0039).

use std::ops::AddAssign;
use std::time::Duration;

/// The work one query or DML statement caused, or the sum over a
/// transaction's statements.
///
/// Every statement counts it as it runs, whether or not a metrics recorder
/// is installed, so an application can log, bill or set quotas on it
/// directly. [`QueryResult::usage`](crate::QueryResult::usage) carries a
/// collected statement's; [`QueryStream::usage`](crate::QueryStream::usage)
/// a streamed one's once it has ended; and
/// [`Session::transaction_usage`](crate::Session::transaction_usage) the
/// sum for the current or last transaction.
///
/// Replication traffic isn't included: one commit's share of a replicated
/// batch isn't well defined, so network cost is measured per node instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Usage {
    /// Rows storage returned to the statement's scans and lookups.
    pub rows_scanned: u64,
    /// Rows the statement returned.
    pub rows_returned: u64,
    /// Bytes of the values its scans and lookups read, as decoded.
    pub bytes_read: u64,
    /// Key and value bytes it added to its transaction's write set: what
    /// its commit writes. Zero for a statement that failed or was cancelled,
    /// whose writes are undone.
    pub bytes_written: u64,
    /// Time spent executing on the statement's task, not counting waits for
    /// storage or for the caller.
    pub cpu: Duration,
    /// The most query memory the statement reserved at once. Over a
    /// transaction, the largest of its statements'.
    pub peak_memory: u64,
    /// Time from the statement's start to its end, including planning and,
    /// for a streamed statement, the caller's time between batches.
    pub elapsed: Duration,
}

impl AddAssign for Usage {
    /// Add `other`'s counts, keeping the larger peak memory.
    fn add_assign(&mut self, other: Self) {
        self.rows_scanned += other.rows_scanned;
        self.rows_returned += other.rows_returned;
        self.bytes_read += other.bytes_read;
        self.bytes_written += other.bytes_written;
        self.cpu += other.cpu;
        self.peak_memory = self.peak_memory.max(other.peak_memory);
        self.elapsed += other.elapsed;
    }
}
