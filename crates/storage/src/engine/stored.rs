//! Tagged value records in the `data` keyspace (ADR-0002).

use bytes::Bytes;

use crate::{Error, Result};

/// Record tag for a deleted key.
const TOMBSTONE: u8 = 0x00;
/// Record tag for a live value; the value bytes follow the tag.
const PUT: u8 = 0x01;

/// A decoded version of a key: a live value or a deletion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StoredValue {
    /// The key holds this value.
    Put(Bytes),
    /// The key was deleted.
    Tombstone,
}

impl StoredValue {
    /// Return the live value, or `None` for a tombstone.
    pub(crate) fn into_value(self) -> Option<Bytes> {
        match self {
            Self::Put(value) => Some(value),
            Self::Tombstone => None,
        }
    }
}

/// Encode a version record: a live value when `value` is `Some`, otherwise a tombstone.
pub(crate) fn encode_stored(value: Option<&[u8]>) -> Vec<u8> {
    match value {
        Some(value) => {
            let mut out = Vec::with_capacity(value.len() + 1);
            out.push(PUT);
            out.extend_from_slice(value);
            out
        }
        None => vec![TOMBSTONE],
    }
}

/// Decode a version record without copying the value bytes.
pub(crate) fn decode_stored(record: Bytes) -> Result<StoredValue> {
    match record.first() {
        Some(&PUT) => Ok(StoredValue::Put(record.slice(1..))),
        Some(&TOMBSTONE) if record.len() == 1 => Ok(StoredValue::Tombstone),
        Some(&tag) => Err(Error::CorruptValue(tag)),
        None => Err(Error::CorruptValue(0xFF)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert puts and tombstones round-trip, and bad tags are rejected.
    fn round_trip_and_corruption() {
        let put = decode_stored(Bytes::from(encode_stored(Some(b"v")))).unwrap();
        assert_eq!(put, StoredValue::Put(Bytes::from_static(b"v")));
        let empty = decode_stored(Bytes::from(encode_stored(Some(b"")))).unwrap();
        assert_eq!(empty, StoredValue::Put(Bytes::new()));
        let gone = decode_stored(Bytes::from(encode_stored(None))).unwrap();
        assert_eq!(gone, StoredValue::Tombstone);
        assert!(matches!(
            decode_stored(Bytes::from_static(&[7])),
            Err(Error::CorruptValue(7))
        ));
        assert!(matches!(
            decode_stored(Bytes::from_static(&[0, 1])),
            Err(Error::CorruptValue(0))
        ));
        assert!(decode_stored(Bytes::new()).is_err());
    }
}
