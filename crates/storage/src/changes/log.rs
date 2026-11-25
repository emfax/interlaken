//! The change log's in-memory part, and the index of its durable part.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{ChangeEntry, ChangeRetention};
use crate::Timestamp;

/// One durable entry's place in the index: its timestamp, bytes and
/// commit time in microseconds.
pub(crate) type DurableMark = (Timestamp, u64, u64);

/// The change log of one replica (ADR-0040).
///
/// Entries are recorded before the commits they hold become visible, so a
/// reader bounded by the visible timestamp never misses one. The *floor*
/// is the newest commit a reader can't replay: following from any
/// timestamp at or after it misses nothing.
#[derive(Debug)]
pub(crate) struct ChangeLog {
    /// What to keep.
    retention: ChangeRetention,
    /// The entries and floors.
    inner: Mutex<Inner>,
}

/// The log's state.
#[derive(Debug)]
struct Inner {
    /// Recent entries, oldest first.
    memory: VecDeque<Arc<ChangeEntry>>,
    /// Bytes the in-memory entries hold.
    memory_bytes: u64,
    /// The newest commit no longer (or never) in memory.
    memory_floor: Timestamp,
    /// The durable entries' index, oldest first.
    durable: VecDeque<DurableMark>,
    /// Bytes the durable entries hold.
    durable_bytes: u64,
    /// The newest commit no longer (or never) in the durable log.
    durable_floor: Timestamp,
}

impl ChangeLog {
    /// Construct a log keeping `retention`, whose memory starts empty at
    /// `last`, the last commit, and whose durable part holds `durable`
    /// above `durable_floor`.
    pub(crate) fn new(
        retention: ChangeRetention,
        last: Timestamp,
        durable_floor: Timestamp,
        durable: Vec<DurableMark>,
    ) -> Self {
        let durable_bytes = durable.iter().map(|(_, bytes, _)| bytes).sum();
        Self {
            retention,
            inner: Mutex::new(Inner {
                memory: VecDeque::new(),
                memory_bytes: 0,
                memory_floor: last,
                durable: durable.into(),
                durable_bytes,
                durable_floor,
            }),
        }
    }

    /// Return what the log keeps.
    pub(crate) fn retention(&self) -> ChangeRetention {
        self.retention
    }

    /// Return `true` if entries are also written durably.
    pub(crate) fn durable(&self) -> bool {
        matches!(self.retention, ChangeRetention::Durable { .. })
    }

    /// Return the state, recovering it from a poisoned lock.
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `entries`, in commit order, which a durable log has already
    /// written. Call it before publishing their timestamps.
    pub(crate) fn record(&self, entries: &[Arc<ChangeEntry>]) {
        if self.retention == ChangeRetention::Off || entries.is_empty() {
            return;
        }
        let limit = self.retention.memory_bytes();
        let durable = self.durable();
        let mut inner = self.inner();
        for entry in entries {
            let bytes = entry.bytes();
            inner.memory.push_back(Arc::clone(entry));
            inner.memory_bytes += bytes;
            if durable {
                inner.durable.push_back((entry.ts, bytes, entry.micros));
                inner.durable_bytes += bytes;
            }
        }
        while inner.memory_bytes > limit {
            let Some(oldest) = inner.memory.pop_front() else {
                break;
            };
            inner.memory_bytes -= oldest.bytes();
            inner.memory_floor = oldest.ts;
        }
    }

    /// Return the floor: following from a timestamp below it would miss
    /// changes.
    pub(crate) fn floor(&self) -> Timestamp {
        let inner = self.inner();
        if self.durable() {
            inner.durable_floor.min(inner.memory_floor)
        } else {
            inner.memory_floor
        }
    }

    /// Return up to `max` entries after `after` and at or before `upto`
    /// from memory, or `None` if memory doesn't reach back to `after`.
    pub(crate) fn memory_after(
        &self,
        after: Timestamp,
        upto: Timestamp,
        max: usize,
    ) -> Option<Vec<Arc<ChangeEntry>>> {
        let inner = self.inner();
        if after < inner.memory_floor {
            return None;
        }
        let start = inner.memory.partition_point(|entry| entry.ts <= after);
        Some(
            inner
                .memory
                .iter()
                .skip(start)
                .take_while(|entry| entry.ts <= upto)
                .take(max)
                .cloned()
                .collect(),
        )
    }

    /// Restart the log after a snapshot install whose last commit is
    /// `last`: the commits before it were never applied here. A durable
    /// log's entries are removed by the install itself.
    pub(crate) fn reset(&self, last: Timestamp) {
        let mut inner = self.inner();
        inner.memory.clear();
        inner.memory_bytes = 0;
        inner.memory_floor = last;
        inner.durable.clear();
        inner.durable_bytes = 0;
        inner.durable_floor = last;
    }

    /// Return the newest durable entry past retention at `now_micros`, if
    /// any: every entry up to it should be removed and the floor raised to
    /// it. Call [`ChangeLog::trimmed`] once they are.
    pub(crate) fn durable_trim(&self, now_micros: u64) -> Option<Timestamp> {
        let ChangeRetention::Durable { age, bytes } = self.retention else {
            return None;
        };
        let age = u64::try_from(age.as_micros()).unwrap_or(u64::MAX);
        let inner = self.inner();
        let mut remaining = inner.durable_bytes;
        let mut through = None;
        for &(ts, size, micros) in &inner.durable {
            if remaining <= bytes && now_micros.saturating_sub(micros) <= age {
                break;
            }
            remaining -= size;
            through = Some(ts);
        }
        through
    }

    /// Note that every durable entry up to `through` was removed.
    pub(crate) fn trimmed(&self, through: Timestamp) {
        let mut inner = self.inner();
        while inner
            .durable
            .front()
            .is_some_and(|(ts, _, _)| *ts <= through)
        {
            let (_, size, _) = inner.durable.pop_front().expect("checked");
            inner.durable_bytes -= size;
        }
        inner.durable_floor = inner.durable_floor.max(through);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;

    use super::*;
    use crate::changes::ChangeWrite;
    use crate::commit::Prior;

    /// Return an entry at `ts` holding `bytes` bytes, recorded at `micros`.
    fn entry(ts: u64, bytes: usize, micros: u64) -> Arc<ChangeEntry> {
        Arc::new(ChangeEntry {
            ts: Timestamp::from_raw(ts),
            micros,
            writes: vec![ChangeWrite {
                key: Bytes::from(vec![0; bytes]),
                value: None,
                prior: Prior::Unknown,
            }],
        })
    }

    /// Return the timestamps of `entries`.
    fn stamps(entries: &[Arc<ChangeEntry>]) -> Vec<u64> {
        entries.iter().map(|e| e.ts.into_raw()).collect()
    }

    #[test]
    /// Assert memory keeps the newest entries within its bytes, and the floor follows.
    fn memory_is_bounded() {
        let ts = Timestamp::from_raw;
        let log = ChangeLog::new(
            ChangeRetention::Memory { bytes: 25 },
            ts(1),
            ts(1),
            Vec::new(),
        );
        log.record(&[entry(2, 10, 0), entry(3, 10, 0)]);
        assert_eq!(stamps(&log.memory_after(ts(1), ts(9), 9).unwrap()), [2, 3]);
        assert_eq!(stamps(&log.memory_after(ts(2), ts(9), 9).unwrap()), [3]);
        assert_eq!(stamps(&log.memory_after(ts(1), ts(2), 9).unwrap()), [2]);
        assert!(log.memory_after(ts(0), ts(9), 9).is_none());
        log.record(&[entry(4, 10, 0)]);
        assert_eq!(log.floor(), ts(2));
        assert!(log.memory_after(ts(1), ts(9), 9).is_none());
        assert_eq!(stamps(&log.memory_after(ts(2), ts(9), 1).unwrap()), [3]);
        log.reset(ts(10));
        assert_eq!(log.floor(), ts(10));
        assert!(log.memory_after(ts(4), ts(20), 9).is_none());
    }

    #[test]
    /// Assert durable trimming honours age and bytes.
    fn durable_trimming() {
        let ts = Timestamp::from_raw;
        let retention = ChangeRetention::Durable {
            age: Duration::from_micros(100),
            bytes: 25,
        };
        let log = ChangeLog::new(retention, ts(1), ts(1), Vec::new());
        log.record(&[entry(2, 10, 0), entry(3, 10, 50), entry(4, 10, 90)]);
        // Over 25 bytes: the oldest goes.
        assert_eq!(log.durable_trim(95), Some(ts(2)));
        log.trimmed(ts(2));
        assert_eq!(log.durable_trim(95), None);
        // Later, entry 3 is older than 100 µs.
        assert_eq!(log.durable_trim(160), Some(ts(3)));
        log.trimmed(ts(3));
        assert_eq!(log.floor(), ts(2), "memory still holds 3 and 4");
    }
}
