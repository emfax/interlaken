//! Interim serial commit path and the commit clock.
//!
//! Milestone M2 commits one transaction at a time under a lock, with no
//! validation, so that reads can be tested against committed data. Milestone
//! M3 replaces this with the commit sequencer (ADR-0005): validation, group
//! commit and durability modes on a dedicated thread. The clock and the
//! `visible_ts` rule stay the same.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::scan::Key;
use crate::txn::WriteOp;
use crate::{Engine, Error, Result, Timestamp};

/// The timestamp source and the visibility watermark (ADR-0004).
#[derive(Debug)]
pub(crate) struct CommitClock {
    /// Last assigned commit timestamp. Held for the duration of a commit.
    last: Mutex<Timestamp>,
    /// Highest commit timestamp whose writes are already in fjall.
    visible: AtomicU64,
}

impl CommitClock {
    /// Construct a clock resuming after `last`, which is already durable.
    pub(crate) fn new(last: Timestamp) -> Self {
        Self {
            last: Mutex::new(last),
            visible: AtomicU64::new(last.into_raw()),
        }
    }

    /// Return the timestamp new snapshots read at.
    pub(crate) fn visible_ts(&self) -> Timestamp {
        Timestamp::from_raw(self.visible.load(Ordering::Acquire))
    }
}

/// Assign the next commit timestamp and write `writes` as one atomic fjall batch.
///
/// `visible_ts` advances only after the batch is in fjall, so no snapshot
/// can observe a timestamp whose data isn't readable yet. Runs on a storage
/// thread.
pub(crate) fn apply(
    engine: &Engine,
    clock: &CommitClock,
    writes: &[(Key, WriteOp)],
) -> Result<Timestamp> {
    let mut last = clock.last.lock().unwrap_or_else(PoisonError::into_inner);
    let ts = last.successor().ok_or(Error::TimestampExhausted)?;
    engine.commit_batch(
        ts,
        writes
            .iter()
            .map(|(key, op)| (&key[..], op.value().map(|v| &v[..]))),
    )?;
    *last = ts;
    clock.visible.store(ts.into_raw(), Ordering::Release);
    Ok(ts)
}
