//! Versioned key layout: `tuple ++ !commit_ts` (ADR-0002).

use super::DecodeError;
use crate::Timestamp;

/// Length of the version suffix appended to every key in the `data` keyspace.
pub const VERSION_SUFFIX_LEN: usize = 8;

/// Append the version suffix for `ts` to an encoded tuple.
///
/// The suffix is the big-endian timestamp with all bits inverted, so versions
/// of one key sort newest first and sit next to each other. A read at
/// snapshot `ts` seeks to `versioned_key(tuple, ts)`; the first entry for that
/// tuple at or after the seek position is the newest version visible at `ts`.
///
/// # Examples
///
/// ```
/// use interlaken_storage::Timestamp;
/// use interlaken_storage::encoding::versioned_key;
///
/// let older = versioned_key(b"k", Timestamp::from_raw(1));
/// let newer = versioned_key(b"k", Timestamp::from_raw(2));
/// assert!(newer < older);
/// ```
#[must_use]
pub fn versioned_key(tuple: &[u8], ts: Timestamp) -> Vec<u8> {
    let mut key = Vec::with_capacity(tuple.len() + VERSION_SUFFIX_LEN);
    key.extend_from_slice(tuple);
    key.extend_from_slice(&(!ts.into_raw()).to_be_bytes());
    key
}

/// Split a versioned key into its encoded tuple and commit timestamp.
///
/// # Errors
///
/// Returns [`DecodeError::MissingVersion`] if the key is shorter than the suffix.
pub fn split_versioned_key(key: &[u8]) -> Result<(&[u8], Timestamp), DecodeError> {
    let Some(split) = key.len().checked_sub(VERSION_SUFFIX_LEN) else {
        return Err(DecodeError::MissingVersion { len: key.len() });
    };
    let (tuple, suffix) = key.split_at(split);
    let mut raw = [0u8; VERSION_SUFFIX_LEN];
    raw.copy_from_slice(suffix);
    Ok((tuple, Timestamp::from_raw(!u64::from_be_bytes(raw))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{Datum, SortOrder, encode_tuple};

    #[test]
    /// Assert splitting inverts appending.
    fn round_trip() {
        let tuple = encode_tuple(&[(Datum::Text("k".into()), SortOrder::ASC)]).unwrap();
        for raw in [0, 1, 42, u64::MAX] {
            let key = versioned_key(&tuple, Timestamp::from_raw(raw));
            assert_eq!(
                split_versioned_key(&key),
                Ok((&tuple[..], Timestamp::from_raw(raw)))
            );
        }
    }

    #[test]
    /// Assert versions of a key sort newest first and stay grouped between neighbouring keys.
    fn versions_sort_newest_first_and_grouped() {
        let tuple = |v| encode_tuple(&[(Datum::Int8(v), SortOrder::ASC)]).unwrap();
        let ts = Timestamp::from_raw;
        let mut keys = [
            versioned_key(&tuple(2), ts(1)),
            versioned_key(&tuple(1), ts(5)),
            versioned_key(&tuple(2), ts(9)),
            versioned_key(&tuple(1), ts(7)),
            versioned_key(&tuple(3), ts(0)),
        ];
        keys.sort();
        let decoded: Vec<_> = keys
            .iter()
            .map(|k| {
                let (t, ts) = split_versioned_key(k).unwrap();
                (t.to_vec(), ts.into_raw())
            })
            .collect();
        assert_eq!(
            decoded,
            vec![
                (tuple(1), 7),
                (tuple(1), 5),
                (tuple(2), 9),
                (tuple(2), 1),
                (tuple(3), 0)
            ]
        );
    }

    #[test]
    /// Assert keys shorter than the suffix are rejected.
    fn short_key() {
        assert_eq!(
            split_versioned_key(&[1, 2, 3]),
            Err(DecodeError::MissingVersion { len: 3 })
        );
    }
}
