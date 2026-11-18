//! The transaction's write buffer, with statement tags (ADR-0004).
//!
//! Reads see only writes from earlier statements, so for each key the buffer
//! keeps the latest write (tagged with its statement) and the write it
//! replaced from an earlier statement. Commit uses the latest write.

use std::collections::BTreeMap;

use bytes::Bytes;

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
}

impl Entry {
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

/// Ordered map of key to buffered write.
#[derive(Clone, Debug, Default)]
pub(crate) struct WriteBuffer {
    entries: BTreeMap<Key, Entry>,
}

impl WriteBuffer {
    /// Record a write to `key` made by `statement`.
    pub(crate) fn write(&mut self, key: Key, op: WriteOp, statement: u64) {
        match self.entries.get_mut(&key) {
            Some(entry) => {
                if entry.latest_statement < statement {
                    entry.earlier = Some(std::mem::replace(&mut entry.latest, op));
                } else {
                    entry.latest = op;
                }
                entry.latest_statement = statement;
            }
            None => {
                self.entries.insert(
                    key,
                    Entry {
                        latest: op,
                        latest_statement: statement,
                        earlier: None,
                    },
                );
            }
        }
    }

    /// Return the buffered write to `key` visible to `statement`, if any.
    pub(crate) fn visible(&self, key: &[u8], statement: u64) -> Option<&WriteOp> {
        self.entries.get(key)?.visible_to(statement)
    }

    /// Return the buffered writes in `range` visible to `statement`, in ascending
    /// key order, as `(key, value)` with `None` for deletes.
    pub(crate) fn visible_range(
        &self,
        range: &KeyRange,
        statement: u64,
    ) -> Vec<(Key, Option<Bytes>)> {
        // `BTreeMap::range` panics on inverted bounds.
        if range.is_empty() {
            return Vec::new();
        }
        self.entries
            .range::<[u8], _>(range.as_slice_bounds())
            .filter_map(|(key, entry)| {
                entry
                    .visible_to(statement)
                    .map(|op| (key.clone(), op.value().cloned()))
            })
            .collect()
    }

    /// Return the final write to each key, for commit.
    pub(crate) fn into_final_writes(self) -> Vec<(Key, WriteOp)> {
        self.entries
            .into_iter()
            .map(|(key, entry)| (key, entry.latest))
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

    #[test]
    /// Assert a statement never sees its own writes but the next one does.
    fn current_statement_writes_are_invisible() {
        let mut buf = WriteBuffer::default();
        buf.write(key("a"), put("1"), 0);
        assert_eq!(buf.visible(b"a", 0), None);
        assert_eq!(buf.visible(b"a", 1), Some(&put("1")));
    }

    #[test]
    /// Assert an overwrite in a later statement hides behind the earlier value until the statement ends.
    fn overwrite_keeps_earlier_value_for_current_statement() {
        let mut buf = WriteBuffer::default();
        buf.write(key("a"), put("1"), 0);
        buf.write(key("a"), put("2"), 2);
        buf.write(key("a"), WriteOp::Delete, 2);
        assert_eq!(buf.visible(b"a", 2), Some(&put("1")));
        assert_eq!(buf.visible(b"a", 3), Some(&WriteOp::Delete));
        assert_eq!(buf.into_final_writes(), vec![(key("a"), WriteOp::Delete)]);
    }

    #[test]
    /// Assert range listings respect bounds and statement visibility.
    fn visible_range_filters() {
        let mut buf = WriteBuffer::default();
        buf.write(key("a"), put("1"), 0);
        buf.write(key("b"), WriteOp::Delete, 0);
        buf.write(key("c"), put("3"), 1);
        let rows = buf.visible_range(&KeyRange::all(), 1);
        assert_eq!(
            rows,
            vec![(key("a"), Some(Bytes::from_static(b"1"))), (key("b"), None)]
        );
        let rows = buf.visible_range(&KeyRange::prefix(b"c"), 2);
        assert_eq!(rows, vec![(key("c"), Some(Bytes::from_static(b"3")))]);
    }
}
