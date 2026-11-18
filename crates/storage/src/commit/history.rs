//! Write sets of recent commits, for backward validation (ADR-0004).

use std::collections::{BTreeMap, VecDeque};

use super::request::CommitRequest;
use crate::scan::{Key, KeyRange};
use crate::txn::ReadSet;
use crate::{Conflict, Timestamp};

/// Keys written by every commit after `floor`, indexed for point and range checks.
#[derive(Debug)]
pub(crate) struct CommitHistory {
    /// Latest commit timestamp that wrote each key.
    latest: BTreeMap<Key, Timestamp>,
    /// Write sets in commit order, for trimming.
    log: VecDeque<(Timestamp, Vec<Key>)>,
    /// Every commit after this timestamp is recorded.
    floor: Timestamp,
}

impl CommitHistory {
    /// Construct an empty history covering commits after `floor`.
    pub(crate) fn new(floor: Timestamp) -> Self {
        Self {
            latest: BTreeMap::new(),
            log: VecDeque::new(),
            floor,
        }
    }

    /// Validate `request` against every commit after its snapshots.
    ///
    /// Writes conflict with a later commit to the same key (first committer
    /// wins); dependencies and reads conflict with a later commit to one of
    /// their keys or to any key inside one of their ranges.
    pub(crate) fn validate(&self, request: &CommitRequest) -> Result<(), Conflict> {
        let oldest = request
            .writes
            .iter()
            .map(|w| w.snapshot)
            .chain((!request.reads.is_empty()).then_some(request.snapshot))
            .chain((!request.dependencies.is_empty()).then_some(request.dependency_snapshot))
            .min();
        if oldest.is_some_and(|ts| ts < self.floor) {
            return Err(Conflict::SnapshotTooOld);
        }
        if request
            .writes
            .iter()
            .any(|w| self.key_changed_after(&w.key, w.snapshot))
        {
            return Err(Conflict::WriteWrite);
        }
        if self.reads_changed_after(&request.dependencies, request.dependency_snapshot) {
            return Err(Conflict::DependencyChanged);
        }
        if self.reads_changed_after(&request.reads, request.snapshot) {
            return Err(Conflict::ReadWrite);
        }
        Ok(())
    }

    /// Return `true` if a commit after `ts` wrote a key in `reads` or inside one of its ranges.
    fn reads_changed_after(&self, reads: &ReadSet, ts: Timestamp) -> bool {
        reads
            .keys()
            .iter()
            .any(|key| self.key_changed_after(key, ts))
            || reads
                .ranges()
                .iter()
                .any(|range| self.range_changed_after(range, ts))
    }

    /// Record that the commit at `ts` wrote `keys`. Timestamps must increase.
    pub(crate) fn record(&mut self, ts: Timestamp, keys: Vec<Key>) {
        for key in &keys {
            self.latest.insert(key.clone(), ts);
        }
        self.log.push_back((ts, keys));
    }

    /// Forget commits at or below `horizon`, which no open snapshot predates.
    pub(crate) fn trim(&mut self, horizon: Timestamp) {
        while let Some((ts, _)) = self.log.front()
            && *ts <= horizon
        {
            let (ts, keys) = self.log.pop_front().expect("front exists");
            for key in keys {
                if self.latest.get(&key) == Some(&ts) {
                    self.latest.remove(&key);
                }
            }
        }
        self.floor = self.floor.max(horizon);
    }

    /// Return the number of commits retained.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.log.len()
    }

    /// Return `true` if a commit after `ts` wrote `key`.
    fn key_changed_after(&self, key: &[u8], ts: Timestamp) -> bool {
        self.latest.get(key).is_some_and(|&written| written > ts)
    }

    /// Return `true` if a commit after `ts` wrote any key in `range`.
    fn range_changed_after(&self, range: &KeyRange, ts: Timestamp) -> bool {
        if range.is_empty() {
            return false;
        }
        self.latest
            .range::<[u8], _>(range.as_slice_bounds())
            .any(|(_, &written)| written > ts)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::commit::WriteIntent;
    use crate::txn::WriteOp;

    /// Construct a key from a static string.
    fn k(s: &'static str) -> Key {
        Bytes::from_static(s.as_bytes())
    }

    /// Construct a timestamp.
    fn ts(raw: u64) -> Timestamp {
        Timestamp::from_raw(raw)
    }

    /// Construct a request at `snapshot` writing `writes` and reading `reads`.
    fn request(snapshot: u64, writes: &[&'static str], reads: ReadSet) -> CommitRequest {
        CommitRequest {
            snapshot: ts(snapshot),
            reads,
            dependencies: ReadSet::default(),
            dependency_snapshot: ts(snapshot),
            writes: writes
                .iter()
                .map(|w| WriteIntent {
                    key: k(w),
                    op: WriteOp::Delete,
                    snapshot: ts(snapshot),
                })
                .collect(),
        }
    }

    #[test]
    /// Assert write-write conflicts only count commits after the snapshot.
    fn first_committer_wins() {
        let mut history = CommitHistory::new(ts(0));
        history.record(ts(2), vec![k("a")]);
        assert_eq!(
            history.validate(&request(1, &["a"], ReadSet::default())),
            Err(Conflict::WriteWrite)
        );
        assert_eq!(
            history.validate(&request(2, &["a"], ReadSet::default())),
            Ok(())
        );
        assert_eq!(
            history.validate(&request(1, &["b"], ReadSet::default())),
            Ok(())
        );
    }

    #[test]
    /// Assert point reads and ranges conflict with later writes inside them.
    fn read_conflicts() {
        let mut history = CommitHistory::new(ts(0));
        history.record(ts(2), vec![k("m")]);
        let mut reads = ReadSet::default();
        reads.record_key(k("m"));
        assert_eq!(
            history.validate(&request(1, &["z"], reads)),
            Err(Conflict::ReadWrite)
        );

        let mut reads = ReadSet::default();
        reads.record_range(KeyRange::prefix(b"n"));
        assert_eq!(history.validate(&request(1, &["z"], reads)), Ok(()));
        let mut reads = ReadSet::default();
        reads.record_range(KeyRange::new(
            std::ops::Bound::Excluded(k("a")),
            std::ops::Bound::Included(k("m")),
        ));
        assert_eq!(
            history.validate(&request(1, &["z"], reads)),
            Err(Conflict::ReadWrite)
        );
    }

    #[test]
    /// Assert trimming forgets old commits but keeps newer writes to the same key.
    fn trim_keeps_newer_writes() {
        let mut history = CommitHistory::new(ts(0));
        history.record(ts(1), vec![k("a"), k("b")]);
        history.record(ts(2), vec![k("a")]);
        history.trim(ts(1));
        assert_eq!(history.len(), 1);
        assert_eq!(
            history.validate(&request(1, &["a"], ReadSet::default())),
            Err(Conflict::WriteWrite)
        );
        assert_eq!(
            history.validate(&request(1, &["b"], ReadSet::default())),
            Ok(())
        );
        assert_eq!(
            history.validate(&request(0, &["b"], ReadSet::default())),
            Err(Conflict::SnapshotTooOld)
        );
    }
}
