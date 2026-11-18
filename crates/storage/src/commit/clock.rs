//! The visibility watermark and the registry of open snapshots.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::Timestamp;

/// `visible_ts` plus the snapshots of every open transaction (ADR-0004, ADR-0008).
///
/// Both the sequencer's validation history and version GC keep everything a
/// pinned snapshot may need, so every transaction holds a [`SnapshotPin`].
/// A pin older than the maximum transaction age stops counting: the next
/// [`horizon`](Self::horizon) revokes it, and the transaction fails its next
/// operation instead of holding back GC indefinitely.
#[derive(Debug)]
pub(crate) struct CommitClock {
    /// Highest commit timestamp whose writes are already in fjall.
    visible: AtomicU64,
    /// Open, unexpired snapshots, keyed by timestamp and a unique pin number.
    pinned: Mutex<Pins>,
    /// Pins revoked for exceeding the maximum age.
    expired: AtomicU64,
}

/// The pin registry.
#[derive(Debug, Default)]
struct Pins {
    by_snapshot: BTreeMap<(Timestamp, u64), Arc<PinState>>,
    next_id: u64,
}

/// State shared between a pin and the registry.
#[derive(Debug)]
pub(crate) struct PinState {
    deadline: Instant,
    revoked: AtomicBool,
}

impl CommitClock {
    /// Construct a clock whose visible timestamp is `last`, which is already durable.
    pub(crate) fn new(last: Timestamp) -> Self {
        Self {
            visible: AtomicU64::new(last.into_raw()),
            pinned: Mutex::new(Pins::default()),
            expired: AtomicU64::new(0),
        }
    }

    /// Return the timestamp new snapshots read at.
    pub(crate) fn visible_ts(&self) -> Timestamp {
        Timestamp::from_raw(self.visible.load(Ordering::Acquire))
    }

    /// Take a snapshot at the visible timestamp and pin it for at most `max_age`.
    ///
    /// The visible timestamp is read under the registry lock, so a
    /// concurrent [`horizon`](Self::horizon) either sees the pin or returns a
    /// value no greater than the snapshot.
    pub(crate) fn pin(self: &Arc<Self>, max_age: Duration) -> SnapshotPin {
        let mut pins = self.pins();
        let ts = self.visible_ts();
        let id = pins.next_id;
        pins.next_id += 1;
        let state = Arc::new(PinState {
            deadline: Instant::now() + max_age,
            revoked: AtomicBool::new(false),
        });
        pins.by_snapshot.insert((ts, id), Arc::clone(&state));
        SnapshotPin {
            clock: Arc::clone(self),
            ts,
            id,
            state,
        }
    }

    /// Advance the visible timestamp to `ts` once its writes are readable.
    ///
    /// Only the sequencer calls this, with increasing timestamps.
    pub(crate) fn publish(&self, ts: Timestamp) {
        self.visible.store(ts.into_raw(), Ordering::Release);
    }

    /// Revoke expired pins and return the oldest snapshot any open or future
    /// transaction can still use.
    ///
    /// Everything at or below the result that is shadowed by a newer version
    /// at or below it is invisible to every live snapshot.
    pub(crate) fn horizon(&self) -> Timestamp {
        let now = Instant::now();
        let mut pins = self.pins();
        let expired: Vec<(Timestamp, u64)> = pins
            .by_snapshot
            .iter()
            .filter(|(_, state)| state.deadline <= now)
            .map(|(key, _)| *key)
            .collect();
        for key in expired {
            if let Some(state) = pins.by_snapshot.remove(&key) {
                // Released before any GC deletion that relies on it (Release
                // pairs with the Acquire in `SnapshotPin::is_revoked`).
                state.revoked.store(true, Ordering::Release);
                self.expired.fetch_add(1, Ordering::Relaxed);
            }
        }
        let visible = self.visible_ts();
        pins.by_snapshot
            .keys()
            .next()
            .map_or(visible, |&(oldest, _)| oldest.min(visible))
    }

    /// Return the number of open, unexpired pins and the oldest pinned snapshot.
    pub(crate) fn active(&self) -> (usize, Option<Timestamp>) {
        let pins = self.pins();
        (
            pins.by_snapshot.len(),
            pins.by_snapshot.keys().next().map(|&(ts, _)| ts),
        )
    }

    /// Return the number of pins revoked for exceeding the maximum age.
    pub(crate) fn expired_count(&self) -> u64 {
        self.expired.load(Ordering::Relaxed)
    }

    /// Lock the pin registry, ignoring poisoning: every critical section leaves it consistent.
    fn pins(&self) -> MutexGuard<'_, Pins> {
        self.pinned.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Keeps validation history and old versions alive for one transaction's snapshot.
#[derive(Debug)]
pub(crate) struct SnapshotPin {
    clock: Arc<CommitClock>,
    ts: Timestamp,
    id: u64,
    state: Arc<PinState>,
}

impl SnapshotPin {
    /// Return the pinned snapshot timestamp.
    pub(crate) fn ts(&self) -> Timestamp {
        self.ts
    }

    /// Return `true` once the pin is past its deadline or has been revoked.
    ///
    /// After this returns `true`, GC may have removed versions the snapshot
    /// needs, so results read since the pin was taken can't be trusted.
    pub(crate) fn is_expired(&self) -> bool {
        self.state.revoked.load(Ordering::Acquire) || Instant::now() >= self.state.deadline
    }
}

impl Drop for SnapshotPin {
    /// Release the pin.
    fn drop(&mut self) {
        self.clock.pins().by_snapshot.remove(&(self.ts, self.id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A maximum age long enough never to expire during a test.
    const LONG: Duration = Duration::from_secs(3600);

    #[test]
    /// Assert the horizon follows the oldest pin and falls back to the visible timestamp.
    fn horizon_tracks_oldest_pin() {
        let clock = Arc::new(CommitClock::new(Timestamp::from_raw(3)));
        assert_eq!(clock.horizon(), Timestamp::from_raw(3));
        let a = clock.pin(LONG);
        let b = clock.pin(LONG);
        clock.publish(Timestamp::from_raw(7));
        let c = clock.pin(LONG);
        assert_eq!(
            (a.ts(), c.ts()),
            (Timestamp::from_raw(3), Timestamp::from_raw(7))
        );
        assert_eq!(clock.horizon(), Timestamp::from_raw(3));
        assert_eq!(clock.active(), (3, Some(Timestamp::from_raw(3))));
        drop(a);
        assert_eq!(clock.horizon(), Timestamp::from_raw(3));
        drop(b);
        assert_eq!(clock.horizon(), Timestamp::from_raw(7));
        drop(c);
        clock.publish(Timestamp::from_raw(9));
        assert_eq!(clock.horizon(), Timestamp::from_raw(9));
    }

    #[test]
    /// Assert expired pins stop holding back the horizon and report themselves expired.
    fn expired_pins_are_revoked() {
        let clock = Arc::new(CommitClock::new(Timestamp::from_raw(1)));
        let old = clock.pin(Duration::ZERO);
        clock.publish(Timestamp::from_raw(5));
        let young = clock.pin(LONG);
        assert!(old.is_expired());
        assert!(!young.is_expired());
        assert_eq!(clock.horizon(), Timestamp::from_raw(5));
        assert_eq!(clock.expired_count(), 1);
        assert_eq!(clock.active().0, 1);
        drop(old);
        assert_eq!(clock.active().0, 1);
    }
}
