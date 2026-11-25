//! The visibility watermark and the registry of open snapshots.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::Timestamp;
use crate::changes::ChangeLog;

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
    /// Tasks waiting for `visible` to reach a timestamp (ADR-0040).
    waiters: Mutex<Waiters>,
    /// The change log, recorded before each publish (ADR-0040).
    changes: ChangeLog,
    /// The newest GC watermark any pass has used: snapshots older than it
    /// may have lost versions, so none can be pinned (ADR-0040).
    gc_floor: AtomicU64,
}

/// Tasks waiting for the visible timestamp, keyed by a unique number.
#[derive(Debug, Default)]
struct Waiters {
    by_id: BTreeMap<u64, (Timestamp, Waker)>,
    next_id: u64,
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
    /// When the pin was taken.
    taken: Instant,
    /// When the pin expires; `None` if the maximum age is too large to represent.
    deadline: Option<Instant>,
    revoked: AtomicBool,
}

impl CommitClock {
    /// Construct a clock whose visible timestamp is `last`, which is
    /// already durable, with no change log.
    #[cfg(test)]
    pub(crate) fn new(last: Timestamp) -> Self {
        Self::with_changes(
            last,
            ChangeLog::new(crate::ChangeRetention::Off, last, last, Vec::new()),
        )
    }

    /// Construct a clock whose visible timestamp is `last`, which is
    /// already durable, recording into `changes`.
    pub(crate) fn with_changes(last: Timestamp, changes: ChangeLog) -> Self {
        Self {
            visible: AtomicU64::new(last.into_raw()),
            pinned: Mutex::new(Pins::default()),
            expired: AtomicU64::new(0),
            waiters: Mutex::new(Waiters::default()),
            changes,
            gc_floor: AtomicU64::new(0),
        }
    }

    /// Raise the GC floor to `ts`: versions older than it may be collected.
    pub(crate) fn raise_gc_floor(&self, ts: Timestamp) {
        self.gc_floor.fetch_max(ts.into_raw(), Ordering::AcqRel);
    }

    /// Return the GC floor: the oldest snapshot that can still be pinned.
    pub(crate) fn gc_floor(&self) -> Timestamp {
        Timestamp::from_raw(self.gc_floor.load(Ordering::Acquire))
    }

    /// Return the watermark a GC pass may collect below, as
    /// [`horizon`](Self::horizon) does, and raise the GC floor to it under
    /// the same registry lock, so [`pin_at`](Self::pin_at) either registers
    /// its pin first, holding the watermark down, or sees the new floor.
    pub(crate) fn gc_horizon(&self) -> Timestamp {
        let mut pins = self.pins();
        let watermark = self.horizon_locked(&mut pins);
        self.raise_gc_floor(watermark);
        watermark
    }

    /// Pin a snapshot at `ts`, which may be older than the visible
    /// timestamp, for at most `max_age`; `None` if GC may already have
    /// collected versions it needs (ADR-0040).
    pub(crate) fn pin_at(
        self: &Arc<Self>,
        ts: Timestamp,
        max_age: Duration,
    ) -> Option<SnapshotPin> {
        let mut pins = self.pins();
        if ts < self.gc_floor() || ts > self.visible_ts() {
            return None;
        }
        let id = pins.next_id;
        pins.next_id += 1;
        let taken = Instant::now();
        let state = Arc::new(PinState {
            taken,
            deadline: taken.checked_add(max_age),
            revoked: AtomicBool::new(false),
        });
        pins.by_snapshot.insert((ts, id), Arc::clone(&state));
        Some(SnapshotPin {
            clock: Arc::clone(self),
            ts,
            id,
            state,
        })
    }

    /// Return the change log.
    pub(crate) fn changes(&self) -> &ChangeLog {
        &self.changes
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
        let taken = Instant::now();
        let state = Arc::new(PinState {
            taken,
            deadline: taken.checked_add(max_age),
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
        // Stored under the waiters' lock, so a waiter that checked the old
        // value has registered before this wakes the satisfied ones.
        let mut waiters = self.waiters.lock().unwrap_or_else(PoisonError::into_inner);
        self.visible.store(ts.into_raw(), Ordering::Release);
        waiters.by_id.retain(|_, (target, waker)| {
            if *target <= ts {
                waker.wake_by_ref();
                false
            } else {
                true
            }
        });
    }

    /// Return a future that resolves once the visible timestamp is at least `ts`.
    pub(crate) fn wait_visible(self: &Arc<Self>, ts: Timestamp) -> WaitVisible {
        WaitVisible {
            clock: Arc::clone(self),
            ts,
            id: None,
        }
    }

    /// Revoke expired pins and return the oldest snapshot any open or future
    /// transaction can still use.
    ///
    /// Everything at or below the result that is shadowed by a newer version
    /// at or below it is invisible to every live snapshot.
    pub(crate) fn horizon(&self) -> Timestamp {
        let mut pins = self.pins();
        self.horizon_locked(&mut pins)
    }

    /// Compute [`horizon`](Self::horizon) with the registry locked.
    fn horizon_locked(&self, pins: &mut Pins) -> Timestamp {
        let now = Instant::now();
        let expired: Vec<(Timestamp, u64)> = pins
            .by_snapshot
            .iter()
            .filter(|(_, state)| state.deadline.is_some_and(|d| d <= now))
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

    /// Revoke every pin whose snapshot is older than `ts`: a replica that
    /// installed a snapshot taken at `ts` no longer holds the versions they
    /// read (ADR-0038). Their transactions fail at their next operation, as
    /// expired ones do.
    pub(crate) fn revoke_before(&self, ts: Timestamp) {
        let mut pins = self.pins();
        self.raise_gc_floor(ts);
        let older: Vec<(Timestamp, u64)> = pins
            .by_snapshot
            .keys()
            .take_while(|(snapshot, _)| *snapshot < ts)
            .copied()
            .collect();
        for key in older {
            if let Some(state) = pins.by_snapshot.remove(&key) {
                state.revoked.store(true, Ordering::Release);
                self.expired.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Return the number of open, unexpired pins and the oldest pinned snapshot.
    pub(crate) fn active(&self) -> (usize, Option<Timestamp>) {
        let pins = self.pins();
        (
            pins.by_snapshot.len(),
            pins.by_snapshot.keys().next().map(|&(ts, _)| ts),
        )
    }

    /// Return how long the oldest open pin has been held, if any is.
    pub(crate) fn oldest_pin_age(&self) -> Option<Duration> {
        let pins = self.pins();
        let oldest = pins.by_snapshot.values().map(|state| state.taken).min()?;
        Some(oldest.elapsed())
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
        self.state.revoked.load(Ordering::Acquire)
            || self.state.deadline.is_some_and(|d| Instant::now() >= d)
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
    /// Assert a maximum age too large for `Instant` means no expiry rather than a panic.
    fn unbounded_age_never_expires() {
        let clock = Arc::new(CommitClock::new(Timestamp::ZERO));
        let pin = clock.pin(Duration::MAX);
        assert!(!pin.is_expired());
        assert_eq!(clock.horizon(), Timestamp::ZERO);
        assert_eq!(clock.expired_count(), 0);
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

    #[test]
    /// Assert revoking before a timestamp revokes exactly the older pins.
    fn revoking_before_a_timestamp() {
        let clock = Arc::new(CommitClock::new(Timestamp::from_raw(3)));
        let old = clock.pin(LONG);
        clock.publish(Timestamp::from_raw(9));
        let new = clock.pin(LONG);
        clock.revoke_before(Timestamp::from_raw(9));
        assert!(old.is_expired());
        assert!(!new.is_expired());
        assert_eq!(clock.active(), (1, Some(Timestamp::from_raw(9))));
        assert_eq!(clock.expired_count(), 1);
    }
}

/// A future that resolves once the visible timestamp reaches a target
/// (ADR-0040). Dropping it unregisters it.
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct WaitVisible {
    /// The clock to watch.
    clock: Arc<CommitClock>,
    /// The timestamp to wait for.
    ts: Timestamp,
    /// The registration among the clock's waiters, once registered.
    id: Option<u64>,
}

impl Future for WaitVisible {
    type Output = ();

    /// Resolve if the target is visible; otherwise register to be woken.
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        if this.clock.visible_ts() >= this.ts {
            this.unregister();
            return Poll::Ready(());
        }
        let mut waiters = this
            .clock
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Checked again under the lock that `publish` stores under.
        if this.clock.visible_ts() >= this.ts {
            if let Some(id) = this.id.take() {
                waiters.by_id.remove(&id);
            }
            return Poll::Ready(());
        }
        let id = *this.id.get_or_insert_with(|| {
            let id = waiters.next_id;
            waiters.next_id += 1;
            id
        });
        waiters.by_id.insert(id, (this.ts, cx.waker().clone()));
        Poll::Pending
    }
}

impl WaitVisible {
    /// Remove this future from the clock's waiters, if it is registered.
    fn unregister(&mut self) {
        if let Some(id) = self.id.take() {
            self.clock
                .waiters
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .by_id
                .remove(&id);
        }
    }
}

impl Drop for WaitVisible {
    /// Unregister a future dropped before it resolved.
    fn drop(&mut self) {
        self.unregister();
    }
}
