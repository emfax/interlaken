//! The reference model: serial MVCC semantics, statement visibility,
//! savepoints and backward validation, written independently of the engine.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Bound, RangeBounds};

use interlaken_storage::{Conflict, Isolation};

use super::scan::{ParityFilter, ScanSpec};

/// Values of one key in order, tagged by timestamp or statement; `None` is a delete.
type VersionLog = Vec<(u64, Option<Vec<u8>>)>;

/// A range over encoded keys.
type Range = (Bound<Vec<u8>>, Bound<Vec<u8>>);

/// The reference store: committed versions per key and every commit's write set.
#[derive(Default)]
pub struct Model {
    versions: BTreeMap<Vec<u8>, VersionLog>,
    commits: Vec<(u64, BTreeSet<Vec<u8>>)>,
    pub last: u64,
}

impl Model {
    /// Return the value of `key` visible at `ts`.
    fn read_at(&self, key: &[u8], ts: u64) -> Option<Vec<u8>> {
        self.versions
            .get(key)?
            .iter()
            .rev()
            .find(|(t, _)| *t <= ts)
            .and_then(|(_, v)| v.clone())
    }

    /// Return the write sets of commits after `ts`.
    fn commits_after(&self, ts: u64) -> impl Iterator<Item = &BTreeSet<Vec<u8>>> {
        self.commits
            .iter()
            .filter(move |(t, _)| *t > ts)
            .map(|(_, keys)| keys)
    }
}

/// One buffered key: its statement-tagged writes and the snapshot of the first.
#[derive(Clone)]
struct BufferedKey {
    log: VersionLog,
    first_snapshot: u64,
}

/// The reference transaction.
pub struct ModelTxn {
    isolation: Isolation,
    read_only: bool,
    snapshot: u64,
    statement: u64,
    writes: BTreeMap<Vec<u8>, BufferedKey>,
    read_keys: BTreeSet<Vec<u8>>,
    read_ranges: Vec<Range>,
    /// Open savepoints, oldest first, with a copy of the writes at that point.
    savepoints: Vec<BTreeMap<Vec<u8>, BufferedKey>>,
}

impl ModelTxn {
    /// Begin at the model's latest commit.
    pub fn begin(model: &Model, isolation: Isolation, read_only: bool) -> Self {
        Self {
            isolation,
            read_only,
            snapshot: model.last,
            statement: 0,
            writes: BTreeMap::new(),
            read_keys: BTreeSet::new(),
            read_ranges: Vec::new(),
            savepoints: Vec::new(),
        }
    }

    /// Return `true` if writes are rejected.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Return the value of `key` this transaction sees, without recording a read.
    fn visible(&self, model: &Model, key: &[u8]) -> Option<Vec<u8>> {
        if let Some(buffered) = self.writes.get(key)
            && let Some((_, value)) = buffered.log.iter().rev().find(|(s, _)| *s < self.statement)
        {
            return value.clone();
        }
        model.read_at(key, self.snapshot)
    }

    /// Read `keys` and record them in the read set.
    pub fn get(&mut self, model: &Model, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
        self.read_keys.extend(keys.iter().cloned());
        keys.iter().map(|k| self.visible(model, k)).collect()
    }

    /// Return every row of a scan in scan order, recording the whole range.
    pub fn scan(&mut self, model: &Model, spec: &ScanSpec) -> Vec<(Vec<u8>, Vec<u8>)> {
        let range = spec.bounds();
        let keys: BTreeSet<&Vec<u8>> = model.versions.keys().chain(self.writes.keys()).collect();
        let filter = spec.parity.map(ParityFilter);
        let mut rows: Vec<_> = keys
            .into_iter()
            .filter(|k| range.contains(*k))
            .filter_map(|k| self.visible(model, k).map(|v| (k.clone(), v)))
            .filter(|(_, v)| filter.as_ref().is_none_or(|f| f.keeps(v)))
            .collect();
        if spec.reverse {
            rows.reverse();
        }
        self.read_ranges.push(range);
        rows
    }

    /// Record a write in the current statement.
    pub fn write(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) {
        let snapshot = self.snapshot;
        self.writes
            .entry(key)
            .or_insert_with(|| BufferedKey {
                log: Vec::new(),
                first_snapshot: snapshot,
            })
            .log
            .push((self.statement, value));
    }

    /// Advance the statement, refreshing the snapshot under READ COMMITTED.
    pub fn next_statement(&mut self, model: &Model) {
        self.statement += 1;
        if self.isolation == Isolation::ReadCommitted {
            self.snapshot = model.last;
        }
    }

    /// Open a savepoint; return its stack position.
    pub fn savepoint(&mut self) -> usize {
        self.savepoints.push(self.writes.clone());
        self.savepoints.len() - 1
    }

    /// Restore the writes saved at stack position `index` and drop later savepoints.
    pub fn rollback_to(&mut self, index: usize) {
        self.writes = self.savepoints[index].clone();
        self.savepoints.truncate(index + 1);
    }

    /// Drop the savepoint at stack position `index` and every later one.
    pub fn release(&mut self, index: usize) {
        self.savepoints.truncate(index);
    }

    /// Return the number of open savepoints.
    pub fn open_savepoints(&self) -> usize {
        self.savepoints.len()
    }

    /// Validate and apply the transaction; return its commit timestamp.
    ///
    /// Writes are checked first (first committer wins), then reads under
    /// SERIALIZABLE, matching the order the engine reports conflicts in.
    pub fn commit(self, model: &mut Model) -> Result<Option<u64>, Conflict> {
        if self.writes.is_empty() {
            return Ok(None);
        }
        for (key, buffered) in &self.writes {
            if model
                .commits_after(buffered.first_snapshot)
                .any(|keys| keys.contains(key))
            {
                return Err(Conflict::WriteWrite);
            }
        }
        if self.isolation == Isolation::Serializable
            && model.commits_after(self.snapshot).any(|keys| {
                keys.iter().any(|k| {
                    self.read_keys.contains(k) || self.read_ranges.iter().any(|r| r.contains(k))
                })
            })
        {
            return Err(Conflict::ReadWrite);
        }
        model.last += 1;
        let ts = model.last;
        let mut written = BTreeSet::new();
        for (key, buffered) in self.writes {
            let value = buffered.log.last().expect("logs are never empty").1.clone();
            model
                .versions
                .entry(key.clone())
                .or_default()
                .push((ts, value));
            written.insert(key);
        }
        model.commits.push((ts, written));
        Ok(Some(ts))
    }
}
