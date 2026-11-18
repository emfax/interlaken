//! MVCC reads at a snapshot timestamp (ADR-0002).
//!
//! Everything here calls fjall synchronously and must only run on storage-pool
//! threads (ADR-0001). Each call opens and closes its own fjall iterator, so no
//! iterator outlives one pool call; resuming a scan needs no held snapshot,
//! because versions newer than the snapshot are ignored by timestamp.

use std::cmp::Ordering;
use std::iter::Peekable;
use std::ops::Bound;
use std::sync::Arc;

use bytes::Bytes;
use fjall::Keyspace;

use super::stored::decode_stored;
use crate::encoding::{split_versioned_key, versioned_key};
use crate::scan::{Key, KeyRange, RowFilter};
use crate::{Result, Timestamp};

/// Upper bound on keys examined by one scan call, including deleted and
/// filtered-out keys, so one call can't monopolise a storage thread.
pub(crate) const MAX_EXAMINED_PER_CALL: usize = 16 * 1024;

/// A key and its value as seen by a transaction; `None` means deleted.
type VisibleRow = (Key, Option<Bytes>);

/// Read the newest version of each key at or below `snapshot`.
pub(crate) fn read_many(
    data: &Keyspace,
    keys: &[Key],
    snapshot: Timestamp,
) -> Result<Vec<Option<Bytes>>> {
    keys.iter()
        .map(|key| read_point(data, key, snapshot))
        .collect()
}

/// Read the newest version of `key` at or below `snapshot`.
///
/// Versions sort newest first, so the visible version is the first entry for
/// `key` at or after `key ++ !snapshot`.
pub(crate) fn read_point(
    data: &Keyspace,
    key: &[u8],
    snapshot: Timestamp,
) -> Result<Option<Bytes>> {
    let first = versioned_key(key, snapshot);
    let last = versioned_key(key, Timestamp::ZERO);
    for guard in data.range(first..=last) {
        let (versioned, record) = guard.into_inner()?;
        let (tuple, _) = split_versioned_key(&versioned)?;
        // Only reachable if the arity rule is broken and a longer key extends `key`.
        if tuple == key {
            return Ok(decode_stored(record.into())?.into_value());
        }
    }
    Ok(None)
}

/// Inputs to one scan call, prepared by the transaction on the async side.
pub(crate) struct ScanJob {
    /// Keys to scan, with any resume key already applied.
    pub(crate) range: KeyRange,
    /// Scan from the end of the range towards the start.
    pub(crate) reverse: bool,
    /// Maximum rows to return (treated as at least one).
    pub(crate) limit_rows: usize,
    /// Stop once returned keys plus values reach this many bytes.
    pub(crate) limit_bytes: usize,
    /// Predicate applied to every candidate row, buffered or stored.
    pub(crate) filter: Option<Arc<dyn RowFilter>>,
    /// Snapshot timestamp to read at.
    pub(crate) snapshot: Timestamp,
    /// The transaction's own writes in `range` that are visible to the
    /// current statement, in ascending key order.
    pub(crate) buffered: Vec<VisibleRow>,
}

/// Result of one scan call.
#[derive(Debug)]
pub(crate) struct ScanOutput {
    /// Rows that passed the filter, in scan order.
    pub(crate) rows: Vec<(Key, Bytes)>,
    /// Last key examined, if the range is not exhausted.
    pub(crate) resume: Option<Key>,
    /// The part of the range this call actually examined, for the read set.
    pub(crate) covered: KeyRange,
}

/// Scan `job.range` at `job.snapshot`, merging the transaction's buffered writes.
pub(crate) fn scan(data: &Keyspace, mut job: ScanJob) -> Result<ScanOutput> {
    if job.reverse {
        job.buffered.reverse();
    }
    let stored = VisibleVersions::new(
        data.range(coarse_bounds(&job.range)),
        &job.range,
        job.snapshot,
        job.reverse,
    );
    let mut merged = Merge {
        stored: stored.peekable(),
        buffered: job.buffered.into_iter().peekable(),
        reverse: job.reverse,
    }
    .peekable();

    let limit_rows = job.limit_rows.max(1);
    let mut rows = Vec::new();
    let mut bytes = 0usize;
    let mut examined = 0usize;
    let mut last = None;
    let mut stopped = false;

    for item in merged.by_ref() {
        let (key, value) = item?;
        examined += 1;
        last = Some(key.clone());
        if let Some(value) = value
            && job.filter.as_ref().is_none_or(|f| f.eval(&key, &value))
        {
            bytes += key.len() + value.len();
            rows.push((key, value));
        }
        if rows.len() >= limit_rows || bytes >= job.limit_bytes || examined >= MAX_EXAMINED_PER_CALL
        {
            stopped = true;
            break;
        }
    }

    let resume = if stopped && merged.peek().is_some() {
        last
    } else {
        None
    };
    let covered = match (&resume, job.reverse) {
        (None, _) => job.range,
        (Some(r), false) => KeyRange::new(job.range.start, Bound::Included(r.clone())),
        (Some(r), true) => KeyRange::new(Bound::Included(r.clone()), job.range.end),
    };
    Ok(ScanOutput {
        rows,
        resume,
        covered,
    })
}

/// Compute fjall bounds over versioned keys that contain every version of
/// every tuple in `range`.
///
/// The bounds may also admit tuples just outside `range` (an excluded start
/// key's own versions, or longer keys extending an included end key); the
/// scan filters those out exactly.
fn coarse_bounds(range: &KeyRange) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let start = match &range.start {
        Bound::Included(s) | Bound::Excluded(s) => Bound::Included(s.to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    };
    let end = match &range.end {
        Bound::Excluded(e) => Bound::Excluded(e.to_vec()),
        // The oldest possible version of `e` is the largest versioned key for it.
        Bound::Included(e) => Bound::Included(versioned_key(e, Timestamp::ZERO)),
        Bound::Unbounded => Bound::Unbounded,
    };
    (start, end)
}

/// Yields the version of each tuple visible at a snapshot, in scan order.
///
/// Tombstones are yielded as `None` so the caller can count them as examined.
struct VisibleVersions<'r> {
    iter: fjall::Iter,
    range: &'r KeyRange,
    snapshot: Timestamp,
    reverse: bool,
    done: bool,
    /// Forward: the tuple whose visible version was already yielded.
    resolved: Option<Bytes>,
    /// Reverse: the current tuple and its newest raw record at or below the snapshot so far.
    pending: Option<(Bytes, Option<Bytes>)>,
}

impl<'r> VisibleVersions<'r> {
    /// Construct the iterator over a fjall range iterator.
    fn new(iter: fjall::Iter, range: &'r KeyRange, snapshot: Timestamp, reverse: bool) -> Self {
        Self {
            iter,
            range,
            snapshot,
            reverse,
            done: false,
            resolved: None,
            pending: None,
        }
    }

    /// Read the next raw entry from fjall in scan order, split into tuple, version and record.
    fn next_entry(&mut self) -> Option<Result<(Bytes, Timestamp, Bytes)>> {
        let guard = if self.reverse {
            self.iter.next_back()
        } else {
            self.iter.next()
        }?;
        Some(guard.into_inner().map_err(Into::into).and_then(|(k, v)| {
            let k = Bytes::from(k);
            let (tuple, ts) = split_versioned_key(&k)?;
            let tuple = k.slice(..tuple.len());
            Ok((tuple, ts, Bytes::from(v)))
        }))
    }

    /// Decode a raw record into a visible row.
    fn emit(tuple: Bytes, record: Bytes) -> Result<VisibleRow> {
        Ok((tuple, decode_stored(record)?.into_value()))
    }

    /// Finish the scan, yielding the last pending reverse tuple if it had a visible version.
    fn finish(&mut self) -> Option<Result<VisibleRow>> {
        self.done = true;
        match self.pending.take() {
            Some((tuple, Some(record))) => Some(Self::emit(tuple, record)),
            _ => None,
        }
    }

    /// Advance a forward scan: the first version at or below the snapshot wins.
    fn next_forward(&mut self) -> Option<Result<VisibleRow>> {
        loop {
            let (tuple, ts, record) = match self.next_entry()? {
                Ok(entry) => entry,
                Err(e) => return Some(Err(e)),
            };
            if !self.range.after_start(&tuple) {
                continue;
            }
            if !self.range.before_end(&tuple) {
                self.done = true;
                return None;
            }
            if self.resolved.as_ref() == Some(&tuple) || ts > self.snapshot {
                continue;
            }
            self.resolved = Some(tuple.clone());
            return Some(Self::emit(tuple, record));
        }
    }

    /// Advance a reverse scan: versions arrive oldest first, so the last one
    /// at or below the snapshot before the tuple changes wins.
    fn next_reverse(&mut self) -> Option<Result<VisibleRow>> {
        loop {
            let (tuple, ts, record) = match self.next_entry() {
                Some(Ok(entry)) => entry,
                Some(Err(e)) => return Some(Err(e)),
                None => return self.finish(),
            };
            if !self.range.before_end(&tuple) {
                continue;
            }
            if !self.range.after_start(&tuple) {
                return self.finish();
            }
            let visible = (ts <= self.snapshot).then_some(record);
            match &mut self.pending {
                Some((current, newest)) if *current == tuple => {
                    if visible.is_some() {
                        *newest = visible;
                    }
                }
                _ => {
                    if let Some((done_tuple, Some(done_record))) =
                        self.pending.replace((tuple, visible))
                    {
                        return Some(Self::emit(done_tuple, done_record));
                    }
                }
            }
        }
    }
}

impl Iterator for VisibleVersions<'_> {
    type Item = Result<VisibleRow>;

    /// Yield the next visible tuple in scan order.
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = if self.reverse {
            self.next_reverse()
        } else {
            self.next_forward()
        };
        if matches!(item, Some(Err(_)) | None) {
            self.done = true;
        }
        item
    }
}

/// Merges stored rows with the transaction's buffered writes; buffered writes win.
struct Merge<S: Iterator<Item = Result<VisibleRow>>> {
    stored: Peekable<S>,
    buffered: Peekable<std::vec::IntoIter<VisibleRow>>,
    reverse: bool,
}

impl<S: Iterator<Item = Result<VisibleRow>>> Iterator for Merge<S> {
    type Item = Result<VisibleRow>;

    /// Yield whichever side's next key comes first in scan order.
    fn next(&mut self) -> Option<Self::Item> {
        let order = match (self.stored.peek(), self.buffered.peek()) {
            (Some(Err(_)), _) => return self.stored.next(),
            (None, None) => return None,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(Ok((stored, _))), Some((buffered, _))) => {
                let order = stored.cmp(buffered);
                if self.reverse { order.reverse() } else { order }
            }
        };
        match order {
            Ordering::Less => self.stored.next(),
            Ordering::Greater => self.buffered.next().map(Ok),
            Ordering::Equal => {
                self.stored.next();
                self.buffered.next().map(Ok)
            }
        }
    }
}
