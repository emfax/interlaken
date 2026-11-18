//! The visibility watermark and the registry of snapshots awaiting validation.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::Timestamp;

/// `visible_ts` plus the snapshots of read-write transactions still open (ADR-0004).
///
/// The sequencer trims its validation history below [`horizon`](Self::horizon),
/// so every transaction that may still commit must hold a [`SnapshotPin`].
#[derive(Debug)]
pub(crate) struct CommitClock {
    /// Highest commit timestamp whose writes are already in fjall.
    visible: AtomicU64,
    /// Open read-write snapshots and how many transactions hold each.
    pinned: Mutex<BTreeMap<Timestamp, usize>>,
}

impl CommitClock {
    /// Construct a clock whose visible timestamp is `last`, which is already durable.
    pub(crate) fn new(last: Timestamp) -> Self {
        Self {
            visible: AtomicU64::new(last.into_raw()),
            pinned: Mutex::new(BTreeMap::new()),
        }
    }

    /// Return the timestamp new snapshots read at.
    pub(crate) fn visible_ts(&self) -> Timestamp {
        Timestamp::from_raw(self.visible.load(Ordering::Acquire))
    }

    /// Take a snapshot at the visible timestamp and pin it until the pin drops.
    ///
    /// The visible timestamp is read under the registry lock, so a
    /// concurrent [`horizon`](Self::horizon) either sees the pin or returns a
    /// value no greater than the snapshot.
    pub(crate) fn pin(self: &Arc<Self>) -> SnapshotPin {
        let mut pinned = self.pinned();
        let ts = self.visible_ts();
        *pinned.entry(ts).or_default() += 1;
        SnapshotPin {
            clock: Arc::clone(self),
            ts,
        }
    }

    /// Advance the visible timestamp to `ts` once its writes are readable.
    ///
    /// Only the sequencer calls this, with increasing timestamps.
    pub(crate) fn publish(&self, ts: Timestamp) {
        self.visible.store(ts.into_raw(), Ordering::Release);
    }

    /// Return the oldest snapshot any open or future read-write transaction can hold.
    pub(crate) fn horizon(&self) -> Timestamp {
        let pinned = self.pinned();
        let visible = self.visible_ts();
        pinned
            .keys()
            .next()
            .map_or(visible, |&oldest| oldest.min(visible))
    }

    /// Lock the pin registry, ignoring poisoning: every critical section leaves it consistent.
    fn pinned(&self) -> MutexGuard<'_, BTreeMap<Timestamp, usize>> {
        self.pinned.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Release one pin at `ts`.
    fn unpin(&self, ts: Timestamp) {
        let mut pinned = self.pinned();
        if let Some(count) = pinned.get_mut(&ts) {
            *count -= 1;
            if *count == 0 {
                pinned.remove(&ts);
            }
        }
    }
}

/// Keeps the commit history from being trimmed past a snapshot while it may still commit.
#[derive(Debug)]
pub(crate) struct SnapshotPin {
    clock: Arc<CommitClock>,
    ts: Timestamp,
}

impl SnapshotPin {
    /// Return the pinned snapshot timestamp.
    pub(crate) fn ts(&self) -> Timestamp {
        self.ts
    }
}

impl Drop for SnapshotPin {
    /// Release the pin.
    fn drop(&mut self) {
        self.clock.unpin(self.ts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert the horizon follows the oldest pin and falls back to the visible timestamp.
    fn horizon_tracks_oldest_pin() {
        let clock = Arc::new(CommitClock::new(Timestamp::from_raw(3)));
        assert_eq!(clock.horizon(), Timestamp::from_raw(3));
        let a = clock.pin();
        let b = clock.pin();
        clock.publish(Timestamp::from_raw(7));
        let c = clock.pin();
        assert_eq!(
            (a.ts(), c.ts()),
            (Timestamp::from_raw(3), Timestamp::from_raw(7))
        );
        assert_eq!(clock.horizon(), Timestamp::from_raw(3));
        drop(a);
        assert_eq!(clock.horizon(), Timestamp::from_raw(3));
        drop(b);
        assert_eq!(clock.horizon(), Timestamp::from_raw(7));
        drop(c);
        clock.publish(Timestamp::from_raw(9));
        assert_eq!(clock.horizon(), Timestamp::from_raw(9));
    }
}
