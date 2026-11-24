//! What a statement read from storage, counted for its usage (ADR-0039).
//!
//! Unlike [`Metrics`](crate::Metrics), which counts per operator and only
//! under `EXPLAIN ANALYZE`, these counters are kept for every statement:
//! scans and lookups add each storage call's rows and bytes, once per
//! call, so the cost is a couple of atomic additions per storage batch.

use std::sync::atomic::{AtomicU64, Ordering};

/// Rows and bytes one statement read from storage, shared by its operators.
#[derive(Debug, Default)]
pub struct UsageCounters {
    /// Rows storage returned to scans and lookups.
    rows_scanned: AtomicU64,
    /// Bytes of the values read, as decoded.
    bytes_read: AtomicU64,
}

impl UsageCounters {
    /// Record one storage call that returned `rows` rows of `bytes` bytes.
    pub(crate) fn record_read(&self, rows: usize, bytes: usize) {
        self.rows_scanned
            .fetch_add(u64::try_from(rows).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.bytes_read
            .fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Return the rows storage has returned to scans and lookups so far.
    #[must_use]
    pub fn rows_scanned(&self) -> u64 {
        self.rows_scanned.load(Ordering::Relaxed)
    }

    /// Return the bytes of the values read so far, as decoded.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read.load(Ordering::Relaxed)
    }
}
