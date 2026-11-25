//! One committed transaction's row writes, and their durable encoding.
//!
//! A durable entry is stored under its commit timestamp, big-endian, with
//! the value (integers big-endian):
//!
//! ```text
//! entry := micros:u64 write_count:u32 write*
//! write := key_len:u32 key tag:u8 (value_len:u32 value)?   ; tags as in the commit log
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::commit::Prior;
use crate::encoding::{Datum, SortOrder, TupleDecoder};
use crate::{Error, Result, Timestamp};

/// One row write of a committed transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeWrite {
    /// The row key: `(table_id, 0, pk...)`.
    pub key: Bytes,
    /// The row's new encoded value, or `None` for a delete.
    pub value: Option<Bytes>,
    /// Whether the key held a row before the transaction.
    pub prior: Prior,
}

impl ChangeWrite {
    /// Return the ID of the table the row belongs to.
    ///
    /// # Panics
    ///
    /// Never for writes in the change log, which holds row keys only.
    #[must_use]
    pub fn table_id(&self) -> i32 {
        row_table(&self.key).expect("the change log holds row keys only")
    }
}

/// One committed transaction's row writes (ADR-0040).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeEntry {
    /// The transaction's commit timestamp.
    pub ts: Timestamp,
    /// When it was recorded, in microseconds since the Unix epoch.
    pub micros: u64,
    /// Its row writes, in key order. Index entries are left out.
    pub writes: Vec<ChangeWrite>,
}

impl ChangeEntry {
    /// Return the bytes of keys and values the entry holds, which
    /// retention counts.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.writes
            .iter()
            .map(|w| (w.key.len() + w.value.as_ref().map_or(0, Bytes::len)) as u64)
            .sum()
    }

    /// Encode the entry's durable value.
    pub(crate) fn encode(&self) -> Bytes {
        let mut out = BytesMut::new();
        out.put_u64(self.micros);
        out.put_u32(u32::try_from(self.writes.len()).expect("write count fits u32"));
        for write in &self.writes {
            out.put_u32(u32::try_from(write.key.len()).expect("key length fits u32"));
            out.put_slice(&write.key);
            let prior = write.prior.code() << 1;
            match &write.value {
                Some(value) => {
                    out.put_u8(prior | 1);
                    out.put_u32(u32::try_from(value.len()).expect("value length fits u32"));
                    out.put_slice(value);
                }
                None => out.put_u8(prior),
            }
        }
        out.freeze()
    }

    /// Decode a durable entry stored at `ts`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptMeta`] if the bytes aren't exactly one entry.
    pub(crate) fn decode(ts: Timestamp, bytes: &[u8]) -> Result<Self> {
        let corrupt = || Error::CorruptMeta("malformed change log entry");
        let mut buf = bytes;
        let need = |buf: &&[u8], n: usize| {
            if buf.remaining() < n {
                Err(corrupt())
            } else {
                Ok(())
            }
        };
        let take = |buf: &mut &[u8]| -> Result<Bytes> {
            need(buf, 4)?;
            let len = usize::try_from(buf.get_u32()).map_err(|_| corrupt())?;
            need(buf, len)?;
            Ok(buf.copy_to_bytes(len))
        };
        need(&buf, 12)?;
        let micros = buf.get_u64();
        let count = buf.get_u32();
        let mut writes = Vec::new();
        for _ in 0..count {
            let key = take(&mut buf)?;
            need(&buf, 1)?;
            let tag = buf.get_u8();
            let prior = Prior::from_code(tag >> 1).ok_or_else(corrupt)?;
            let value = match tag & 1 {
                0 => None,
                _ => Some(take(&mut buf)?),
            };
            writes.push(ChangeWrite { key, value, prior });
        }
        if buf.has_remaining() {
            return Err(corrupt());
        }
        Ok(Self { ts, micros, writes })
    }
}

/// Return the table ID of a row key `(table_id, 0, pk...)`, or `None` for
/// an index entry or any other key.
fn row_table(key: &[u8]) -> Option<i32> {
    let mut columns = TupleDecoder::new(key);
    let table = match columns.next()? {
        Ok((Datum::Int4(table), order)) if order == SortOrder::ASC => table,
        _ => return None,
    };
    match columns.next()? {
        Ok((Datum::Int4(0), order)) if order == SortOrder::ASC => Some(table),
        _ => None,
    }
}

/// Return the current time in microseconds since the Unix epoch.
fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// Build the change entries of committed writes `(ts, key, value, prior)`,
/// in commit order: one per transaction that wrote a row. Writes to one
/// transaction are contiguous and share its timestamp.
pub(crate) fn entries_from<'a>(
    writes: impl IntoIterator<Item = (Timestamp, &'a Bytes, Option<&'a Bytes>, Prior)>,
) -> Vec<std::sync::Arc<ChangeEntry>> {
    let micros = now_micros();
    let mut out: Vec<ChangeEntry> = Vec::new();
    for (ts, key, value, prior) in writes {
        if row_table(key).is_none() {
            continue;
        }
        let write = ChangeWrite {
            key: key.clone(),
            value: value.cloned(),
            prior,
        };
        match out.last_mut() {
            Some(entry) if entry.ts == ts => entry.writes.push(write),
            _ => out.push(ChangeEntry {
                ts,
                micros,
                writes: vec![write],
            }),
        }
    }
    out.into_iter().map(std::sync::Arc::new).collect()
}

/// Return the durable key of the entry at `ts`.
pub(crate) fn durable_key(ts: Timestamp) -> [u8; 8] {
    ts.into_raw().to_be_bytes()
}

/// Return the timestamp of a durable key.
///
/// # Errors
///
/// Returns [`Error::CorruptMeta`] if the key isn't 8 bytes.
pub(crate) fn decode_durable_key(key: &[u8]) -> Result<Timestamp> {
    let bytes: [u8; 8] = key
        .try_into()
        .map_err(|_| Error::CorruptMeta("change log key is not 8 bytes"))?;
    Ok(Timestamp::from_raw(u64::from_be_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::KeyBuilder;

    /// Return a key of `table` and `index` with one more column.
    fn key(table: i32, index: i32) -> Bytes {
        let mut builder = KeyBuilder::new();
        builder
            .push(&Datum::Int4(table), SortOrder::ASC)
            .push(&Datum::Int4(index), SortOrder::ASC)
            .push(&Datum::Int8(7), SortOrder::ASC);
        Bytes::from(builder.finish().unwrap())
    }

    #[test]
    /// Assert only row keys are kept, grouped by transaction, and entries round-trip.
    fn entries_keep_rows_and_round_trip() {
        let (row, index, other) = (key(100, 0), key(100, 2), Bytes::from_static(b"meta"));
        let value = Bytes::from_static(b"v");
        let ts = |n| Timestamp::from_raw(n);
        let entries = entries_from([
            (ts(5), &row, Some(&value), Prior::Absent),
            (ts(5), &index, Some(&value), Prior::Unknown),
            (ts(6), &other, None, Prior::Unknown),
            (ts(7), &row, None, Prior::Existed),
        ]);
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[0].ts, entries[0].writes.len()), (ts(5), 1));
        assert_eq!(entries[0].writes[0].table_id(), 100);
        assert_eq!(entries[1].writes[0].value, None);
        for entry in &entries {
            let decoded = ChangeEntry::decode(entry.ts, &entry.encode()).unwrap();
            assert_eq!(&decoded, entry.as_ref());
        }
        assert!(ChangeEntry::decode(ts(5), &entries[0].encode()[..5]).is_err());
    }
}
