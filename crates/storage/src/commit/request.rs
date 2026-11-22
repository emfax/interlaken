//! What a transaction sends to the sequencer at commit.
//!
//! A [`CommitRequest`] can be encoded, so a replica that isn't the leader
//! can send it to the one that is (ADR-0037). Encoding, integers big-endian:
//!
//! ```text
//! request := snapshot:u64 dependency_snapshot:u64 reads:set dependencies:set
//!            write_count:u32 write*
//! set     := key_count:u32 bytes* range_count:u32 (bound bound)*
//! bound   := 0 | 1 bytes | 2 bytes          ; unbounded, included, excluded
//! write   := bytes (0 | 1 bytes) snapshot:u64   ; delete, or put with a value
//! bytes   := len:u32 byte*
//! ```

use std::ops::Bound;

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::scan::{Key, KeyRange};
use crate::txn::{ReadSet, WriteOp};
use crate::{Error, Result, Timestamp};

/// One buffered write, with the snapshot it must be validated against.
#[derive(Clone, Debug)]
pub(crate) struct WriteIntent {
    /// Key written.
    pub(crate) key: Key,
    /// Final write to the key.
    pub(crate) op: WriteOp,
    /// Snapshot of the statement that first wrote the key. A commit after
    /// this timestamp that also wrote the key is a lost update.
    pub(crate) snapshot: Timestamp,
}

/// A transaction ready for validation: its snapshots, what it read and
/// registered as dependencies, and what it writes.
///
/// Opaque outside storage, except that it can be encoded and decoded, so a
/// replicated commit path can send it to the replica that validates.
#[derive(Debug)]
pub struct CommitRequest {
    /// Snapshot the read set was taken at.
    pub(crate) snapshot: Timestamp,
    /// Reads to validate. The transaction leaves this empty unless it is
    /// SERIALIZABLE, so validation needs no isolation level.
    pub(crate) reads: ReadSet,
    /// Dependencies, validated at every isolation level.
    pub(crate) dependencies: ReadSet,
    /// Snapshot at which the first dependency was registered. Under READ
    /// COMMITTED the snapshot moves forward afterwards, so dependencies are
    /// validated against this, not `snapshot`.
    pub(crate) dependency_snapshot: Timestamp,
    /// Writes to validate and apply. Never empty.
    pub(crate) writes: Vec<WriteIntent>,
}

/// Return the corruption error for a malformed request.
fn malformed() -> Error {
    Error::CorruptMeta("malformed commit request")
}

/// Append `bytes` with its length.
fn put_bytes(out: &mut BytesMut, bytes: &[u8]) {
    out.put_u32(u32::try_from(bytes.len()).expect("key or value length fits u32"));
    out.put_slice(bytes);
}

/// Read length-prefixed bytes.
fn get_bytes(buf: &mut &[u8]) -> Result<Bytes> {
    if buf.remaining() < 4 {
        return Err(malformed());
    }
    let len = usize::try_from(buf.get_u32()).map_err(|_| malformed())?;
    if buf.remaining() < len {
        return Err(malformed());
    }
    Ok(buf.copy_to_bytes(len))
}

/// Read one byte, a `u32` or a `u64`, failing on a short buffer.
fn get_u8(buf: &mut &[u8]) -> Result<u8> {
    if buf.remaining() < 1 {
        return Err(malformed());
    }
    Ok(buf.get_u8())
}

/// Read a big-endian `u32`.
fn get_u32(buf: &mut &[u8]) -> Result<u32> {
    if buf.remaining() < 4 {
        return Err(malformed());
    }
    Ok(buf.get_u32())
}

/// Read a big-endian `u64`.
fn get_u64(buf: &mut &[u8]) -> Result<u64> {
    if buf.remaining() < 8 {
        return Err(malformed());
    }
    Ok(buf.get_u64())
}

/// Append a range bound.
fn put_bound(out: &mut BytesMut, bound: &Bound<Key>) {
    match bound {
        Bound::Unbounded => out.put_u8(0),
        Bound::Included(key) => {
            out.put_u8(1);
            put_bytes(out, key);
        }
        Bound::Excluded(key) => {
            out.put_u8(2);
            put_bytes(out, key);
        }
    }
}

/// Read a range bound.
fn get_bound(buf: &mut &[u8]) -> Result<Bound<Key>> {
    Ok(match get_u8(buf)? {
        0 => Bound::Unbounded,
        1 => Bound::Included(get_bytes(buf)?),
        2 => Bound::Excluded(get_bytes(buf)?),
        _ => return Err(malformed()),
    })
}

/// Append a read set.
fn put_set(out: &mut BytesMut, set: &ReadSet) {
    out.put_u32(u32::try_from(set.keys().len()).expect("key count fits u32"));
    for key in set.keys() {
        put_bytes(out, key);
    }
    out.put_u32(u32::try_from(set.ranges().len()).expect("range count fits u32"));
    for range in set.ranges() {
        put_bound(out, &range.start);
        put_bound(out, &range.end);
    }
}

/// Read a read set.
fn get_set(buf: &mut &[u8]) -> Result<ReadSet> {
    let mut set = ReadSet::default();
    for _ in 0..get_u32(buf)? {
        set.record_key(get_bytes(buf)?);
    }
    for _ in 0..get_u32(buf)? {
        let start = get_bound(buf)?;
        let end = get_bound(buf)?;
        set.record_range(KeyRange::new(start, end));
    }
    Ok(set)
}

impl CommitRequest {
    /// Encode the request, to send it to the replica that validates commits.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::new();
        out.put_u64(self.snapshot.into_raw());
        out.put_u64(self.dependency_snapshot.into_raw());
        put_set(&mut out, &self.reads);
        put_set(&mut out, &self.dependencies);
        out.put_u32(u32::try_from(self.writes.len()).expect("write count fits u32"));
        for write in &self.writes {
            put_bytes(&mut out, &write.key);
            match &write.op {
                WriteOp::Delete => out.put_u8(0),
                WriteOp::Put(value) => {
                    out.put_u8(1);
                    put_bytes(&mut out, value);
                }
            }
            out.put_u64(write.snapshot.into_raw());
        }
        out.freeze()
    }

    /// Decode a request encoded by [`CommitRequest::encode`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptMeta`] if the bytes aren't exactly one request
    /// with at least one write.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut buf = bytes;
        let snapshot = Timestamp::from_raw(get_u64(&mut buf)?);
        let dependency_snapshot = Timestamp::from_raw(get_u64(&mut buf)?);
        let reads = get_set(&mut buf)?;
        let dependencies = get_set(&mut buf)?;
        let mut writes = Vec::new();
        for _ in 0..get_u32(&mut buf)? {
            let key = get_bytes(&mut buf)?;
            let op = match get_u8(&mut buf)? {
                0 => WriteOp::Delete,
                1 => WriteOp::Put(get_bytes(&mut buf)?),
                _ => return Err(malformed()),
            };
            let snapshot = Timestamp::from_raw(get_u64(&mut buf)?);
            writes.push(WriteIntent { key, op, snapshot });
        }
        if buf.has_remaining() || writes.is_empty() {
            return Err(malformed());
        }
        Ok(Self {
            snapshot,
            reads,
            dependencies,
            dependency_snapshot,
            writes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert requests round-trip through their encoding and damage is refused.
    fn requests_round_trip() {
        let mut reads = ReadSet::default();
        reads.record_key(Bytes::from_static(b"r"));
        reads.record_range(KeyRange::new(
            Bound::Included(Bytes::from_static(b"a")),
            Bound::Excluded(Bytes::from_static(b"m")),
        ));
        let mut dependencies = ReadSet::default();
        dependencies.record_range(KeyRange::new(Bound::Unbounded, Bound::Unbounded));
        let request = CommitRequest {
            snapshot: Timestamp::from_raw(9),
            reads,
            dependencies,
            dependency_snapshot: Timestamp::from_raw(7),
            writes: vec![
                WriteIntent {
                    key: Bytes::from_static(b"k"),
                    op: WriteOp::Put(Bytes::from_static(b"v")),
                    snapshot: Timestamp::from_raw(8),
                },
                WriteIntent {
                    key: Bytes::from_static(b"d"),
                    op: WriteOp::Delete,
                    snapshot: Timestamp::from_raw(9),
                },
            ],
        };
        let bytes = request.encode();
        let decoded = CommitRequest::decode(&bytes).unwrap();
        assert_eq!(decoded.encode(), bytes);
        assert_eq!(decoded.writes.len(), 2);
        assert_eq!(decoded.reads.ranges().len(), 1);
        for len in 0..bytes.len() {
            assert!(
                CommitRequest::decode(&bytes[..len]).is_err(),
                "prefix of {len}"
            );
        }
    }
}
