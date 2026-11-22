//! Entries of the commit log (ADR-0037).
//!
//! Under the log commit path, each validated commit group becomes one log
//! entry before any of it reaches the `data` keyspace. The entry holds
//! everything applying it needs: its index, the timestamp of its first
//! transaction, and each transaction's writes in order. The `i`th
//! transaction of the entry commits at `first_ts + i`, so every replica
//! applying the same log writes the same versions.
//!
//! Encoding, all integers big-endian:
//!
//! ```text
//! entry := index:u64 first_ts:u64 txn_count:u32 txn*
//! txn   := write_count:u32 write*
//! write := key_len:u32 key tag:u8 (value_len:u32 value)?   ; tag 1 = put, 0 = delete
//! ```

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::{Error, Result, Timestamp};

/// One write of a logged transaction: a key and the value it leaves, or `None` for a delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogWrite {
    /// The key written.
    pub(crate) key: Bytes,
    /// The value written, or `None` for a delete.
    pub(crate) value: Option<Bytes>,
}

/// One commit group, as the log holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogEntry {
    /// The entry's position in the log, from 1.
    pub(crate) index: u64,
    /// The commit timestamp of the first transaction.
    pub(crate) first_ts: Timestamp,
    /// Each transaction's writes, in commit order.
    pub(crate) txns: Vec<Vec<LogWrite>>,
}

/// Return the corruption error for a malformed entry.
fn corrupt() -> Error {
    Error::CorruptMeta("malformed commit log entry")
}

impl LogEntry {
    /// Return the commit timestamp of the last transaction.
    ///
    /// # Panics
    ///
    /// Panics if the entry has no transactions, which the sequencer never logs.
    pub(crate) fn last_ts(&self) -> Timestamp {
        let count = u64::try_from(self.txns.len()).expect("transaction count fits u64");
        Timestamp::from_raw(self.first_ts.into_raw() + count - 1)
    }

    /// Return each write with the timestamp it commits at.
    pub(crate) fn versions(&self) -> impl Iterator<Item = (Timestamp, &LogWrite)> {
        self.txns
            .iter()
            .zip(self.first_ts.into_raw()..)
            .flat_map(|(writes, ts)| writes.iter().map(move |w| (Timestamp::from_raw(ts), w)))
    }

    /// Encode the entry.
    pub(crate) fn encode(&self) -> Bytes {
        let mut out = BytesMut::new();
        out.put_u64(self.index);
        out.put_u64(self.first_ts.into_raw());
        out.put_u32(u32::try_from(self.txns.len()).expect("transaction count fits u32"));
        for writes in &self.txns {
            out.put_u32(u32::try_from(writes.len()).expect("write count fits u32"));
            for write in writes {
                out.put_u32(u32::try_from(write.key.len()).expect("key length fits u32"));
                out.put_slice(&write.key);
                match &write.value {
                    Some(value) => {
                        out.put_u8(1);
                        out.put_u32(u32::try_from(value.len()).expect("value length fits u32"));
                        out.put_slice(value);
                    }
                    None => out.put_u8(0),
                }
            }
        }
        out.freeze()
    }

    /// Decode an entry.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptMeta`] if the bytes aren't exactly one entry.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
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
        need(&buf, 20)?;
        let index = buf.get_u64();
        let first_ts = Timestamp::from_raw(buf.get_u64());
        let txn_count = buf.get_u32();
        let mut txns = Vec::new();
        for _ in 0..txn_count {
            need(&buf, 4)?;
            let write_count = buf.get_u32();
            let mut writes = Vec::new();
            for _ in 0..write_count {
                let key = take(&mut buf)?;
                need(&buf, 1)?;
                let value = match buf.get_u8() {
                    0 => None,
                    1 => Some(take(&mut buf)?),
                    _ => return Err(corrupt()),
                };
                writes.push(LogWrite { key, value });
            }
            txns.push(writes);
        }
        if buf.has_remaining() || txns.is_empty() {
            return Err(corrupt());
        }
        Ok(Self {
            index,
            first_ts,
            txns,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a write.
    fn write(key: &str, value: Option<&str>) -> LogWrite {
        LogWrite {
            key: Bytes::copy_from_slice(key.as_bytes()),
            value: value.map(|v| Bytes::copy_from_slice(v.as_bytes())),
        }
    }

    #[test]
    /// Assert entries round-trip, give each transaction its timestamp, and reject damage.
    fn entries_round_trip() {
        let entry = LogEntry {
            index: 7,
            first_ts: Timestamp::from_raw(41),
            txns: vec![
                vec![write("a", Some("1")), write("b", None)],
                vec![write("", Some(""))],
            ],
        };
        let bytes = entry.encode();
        assert_eq!(LogEntry::decode(&bytes).unwrap(), entry);
        assert_eq!(entry.last_ts(), Timestamp::from_raw(42));
        let versions: Vec<(u64, &[u8])> = entry
            .versions()
            .map(|(ts, w)| (ts.into_raw(), &w.key[..]))
            .collect();
        assert_eq!(versions, [(41, &b"a"[..]), (41, b"b"), (42, b"")]);
        for len in 0..bytes.len() {
            assert!(
                LogEntry::decode(&bytes[..len]).is_err(),
                "prefix of {len} bytes"
            );
        }
        let mut extra = bytes.to_vec();
        extra.push(0);
        assert!(LogEntry::decode(&extra).is_err());
    }
}
