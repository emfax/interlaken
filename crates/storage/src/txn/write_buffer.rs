//! The transaction's write buffer, with statement tags and an undo log (ADR-0004).
//!
//! Reads see only writes from earlier statements, so for each key the buffer
//! keeps the latest write (tagged with its statement) and the write it
//! replaced from an earlier statement. Commit uses the latest write.
//!
//! While a savepoint is open, every write first records the key's previous
//! state in an undo log; rolling back replays the log backwards to a
//! savepoint's position, regardless of statement tags.

use std::collections::BTreeMap;

use bytes::Bytes;

use crate::Timestamp;
use crate::commit::WriteIntent;
use crate::scan::{Key, KeyRange};

/// A buffered write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WriteOp {
    /// Set the key to this value.
    Put(Bytes),
    /// Delete the key.
    Delete,
}

impl WriteOp {
    /// Return the value this write leaves behind, or `None` for a delete.
    pub(crate) fn value(&self) -> Option<&Bytes> {
        match self {
            Self::Put(value) => Some(value),
            Self::Delete => None,
        }
    }
}

/// Buffered state of one key.
#[derive(Clone, Debug)]
struct Entry {
    /// Most recent write.
    latest: WriteOp,
    /// Statement that made `latest`.
    latest_statement: u64,
    /// The write `latest` replaced, if it came from an earlier statement.
    earlier: Option<WriteOp>,
    /// Snapshot of the statement that first wrote the key, for validation.
    snapshot: Timestamp,
}

impl Entry {
    /// Return the bytes the final write to `key` adds to the write set.
    fn size(&self, key: &[u8]) -> usize {
        key.len() + self.latest.value().map_or(0, Bytes::len)
    }

    /// Return the write visible to `statement`: the latest if it came from an
    /// earlier statement, otherwise whatever an earlier statement left.
    fn visible_to(&self, statement: u64) -> Option<&WriteOp> {
        if self.latest_statement < statement {
            Some(&self.latest)
        } else {
            self.earlier.as_ref()
        }
    }
}

/// Ordered map of key to buffered write, plus the undo log for savepoints.
#[derive(Clone, Debug, Default)]
pub(crate) struct WriteBuffer {
    entries: BTreeMap<Key, Entry>,
    /// Previous state of each key written while recording, oldest first.
    undo: Vec<(Key, Option<Entry>)>,
    /// Record undo entries; set while any savepoint is open.
    recording: bool,
    /// Total size of the final writes: keys plus values.
    bytes: usize,
}

impl WriteBuffer {
    /// Record a write to `key` made by `statement` reading at `snapshot`.
    pub(crate) fn write(&mut self, key: Key, op: WriteOp, statement: u64, snapshot: Timestamp) {
        self.bytes = self.bytes_after(&key, &op);
        let previous = self.entries.get(&key).cloned();
        if self.recording {
            self.undo.push((key.clone(), previous.clone()));
        }
        let entry = match previous {
            Some(mut entry) => {
                if entry.latest_statement < statement {
                    entry.earlier = Some(std::mem::replace(&mut entry.latest, op));
                } else {
                    entry.latest = op;
                }
                entry.latest_statement = statement;
                entry
            }
            None => Entry {
                latest: op,
                latest_statement: statement,
                earlier: None,
                snapshot,
            },
        };
        self.entries.insert(key, entry);
    }

    /// Return the write set size, in bytes of keys plus values, after writing `op` to `key`.
    pub(crate) fn bytes_after(&self, key: &[u8], op: &WriteOp) -> usize {
        let old = self.entries.get(key).map_or(0, |e| e.size(key));
        self.bytes - old + key.len() + op.value().map_or(0, Bytes::len)
    }

    /// Return the write set size in bytes of keys plus values.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Return the buffered write to `key` visible to `statement`, if any.
    pub(crate) fn visible(&self, key: &[u8], statement: u64) -> Option<&WriteOp> {
        self.entries.get(key)?.visible_to(statement)
    }

    /// Return the latest buffered write to `key`, including the current statement's.
    pub(crate) fn latest(&self, key: &[u8]) -> Option<&WriteOp> {
        self.entries.get(key).map(|entry| &entry.latest)
    }

    /// Return the buffered writes in `range` visible to `statement`, in ascending
    /// key order, as `(key, value)` with `None` for deletes.
    ///
    /// With `statement` of `None`, return the latest write to each key,
    /// including the current statement's.
    pub(crate) fn visible_range(
        &self,
        range: &KeyRange,
        statement: Option<u64>,
    ) -> Vec<(Key, Option<Bytes>)> {
        // `BTreeMap::range` panics on inverted bounds.
        if range.is_empty() {
            return Vec::new();
        }
        self.entries
            .range::<[u8], _>(range.as_slice_bounds())
            .filter_map(|(key, entry)| {
                let op = match statement {
                    Some(statement) => entry.visible_to(statement),
                    None => Some(&entry.latest),
                };
                op.map(|op| (key.clone(), op.value().cloned()))
            })
            .collect()
    }

    /// Start recording undo entries and return the current undo position.
    pub(crate) fn mark(&mut self) -> usize {
        self.recording = true;
        self.undo.len()
    }

    /// Restore every key written since undo position `mark`.
    pub(crate) fn rollback_to(&mut self, mark: usize) {
        while self.undo.len() > mark {
            let (key, previous) = self.undo.pop().expect("length checked");
            let new_size = previous.as_ref().map_or(0, |e| e.size(&key));
            let replaced = match previous {
                Some(entry) => self.entries.insert(key.clone(), entry),
                None => self.entries.remove(&key),
            };
            self.bytes = self.bytes - replaced.map_or(0, |e| e.size(&key)) + new_size;
        }
    }

    /// Stop recording and discard the undo log, once no savepoint is open.
    pub(crate) fn clear_undo(&mut self) {
        self.recording = false;
        self.undo = Vec::new();
    }

    /// Return the final write to each key with its validation snapshot, for commit.
    pub(crate) fn into_intents(self) -> Vec<WriteIntent> {
        self.entries
            .into_iter()
            .map(|(key, entry)| WriteIntent {
                key,
                op: entry.latest,
                snapshot: entry.snapshot,
            })
            .collect()
    }

    /// Return `true` if nothing has been written.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a put of `v`.
    fn put(v: &'static str) -> WriteOp {
        WriteOp::Put(Bytes::from_static(v.as_bytes()))
    }

    /// Construct a key from a static string.
    fn key(k: &'static str) -> Key {
        Bytes::from_static(k.as_bytes())
    }

    /// Construct a timestamp.
    fn ts(raw: u64) -> Timestamp {
        Timestamp::from_raw(raw)
    }

    #[test]
    /// Assert a statement never sees its own writes but the next one does.
    fn current_statement_writes_are_invisible() {
        let mut buf = WriteBuffer::default();
        buf.write(key("a"), put("1"), 0, ts(0));
        assert_eq!(buf.visible(b"a", 0), None);
        assert_eq!(buf.visible(b"a", 1), Some(&put("1")));
    }

    #[test]
    /// Assert an overwrite in a later statement hides behind the earlier value until the statement ends.
    fn overwrite_keeps_earlier_value_for_current_statement() {
        let mut buf = WriteBuffer::default();
        buf.write(key("a"), put("1"), 0, ts(1));
        buf.write(key("a"), put("2"), 2, ts(5));
        buf.write(key("a"), WriteOp::Delete, 2, ts(5));
        assert_eq!(buf.visible(b"a", 2), Some(&put("1")));
        assert_eq!(buf.visible(b"a", 3), Some(&WriteOp::Delete));
        let intents = buf.into_intents();
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].op, WriteOp::Delete);
        assert_eq!(intents[0].snapshot, ts(1), "first write's snapshot is kept");
    }

    #[test]
    /// Assert range listings respect bounds and statement visibility.
    fn visible_range_filters() {
        let mut buf = WriteBuffer::default();
        buf.write(key("a"), put("1"), 0, ts(0));
        buf.write(key("b"), WriteOp::Delete, 0, ts(0));
        buf.write(key("c"), put("3"), 1, ts(0));
        let rows = buf.visible_range(&KeyRange::all(), Some(1));
        assert_eq!(
            rows,
            vec![(key("a"), Some(Bytes::from_static(b"1"))), (key("b"), None)]
        );
        let rows = buf.visible_range(&KeyRange::prefix(b"c"), Some(2));
        assert_eq!(rows, vec![(key("c"), Some(Bytes::from_static(b"3")))]);
        assert_eq!(buf.visible_range(&KeyRange::prefix(b"c"), Some(1)), vec![]);
        assert_eq!(buf.visible_range(&KeyRange::all(), None).len(), 3);
    }

    #[test]
    /// Assert rollback restores overwritten and removes newly written keys, across statements.
    fn rollback_restores_previous_state() {
        let mut buf = WriteBuffer::default();
        buf.write(key("a"), put("1"), 0, ts(1));
        let outer = buf.mark();
        buf.write(key("a"), put("2"), 1, ts(2));
        let inner = buf.mark();
        buf.write(key("b"), put("3"), 2, ts(3));
        buf.rollback_to(inner);
        assert_eq!(buf.visible(b"b", 9), None);
        assert_eq!(buf.visible(b"a", 9), Some(&put("2")));
        buf.write(key("b"), put("4"), 2, ts(3));
        assert_eq!(buf.bytes(), 4);
        buf.rollback_to(outer);
        assert_eq!(buf.visible(b"a", 9), Some(&put("1")));
        assert_eq!(buf.visible(b"b", 9), None);
        assert_eq!(buf.bytes(), 2, "only a=1 is left");
        buf.clear_undo();
        buf.write(key("c"), put("5"), 3, ts(3));
        assert!(buf.undo.is_empty());
    }
}
