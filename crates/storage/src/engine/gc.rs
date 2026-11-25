//! Removing versions no snapshot can see (ADR-0008).
//!
//! For each key, versions sort newest first. Given a watermark `w` no live
//! snapshot is older than, let `v` be the newest version at or below `w`:
//! every snapshot sees `v` or something newer, so every version older than
//! `v` is obsolete. If `v` is a tombstone it can go too, since with nothing
//! older left, a snapshot that would have seen it sees no value either way.
//! Versions above `w` are never touched, and no new version can appear at or
//! below `w`, so GC never races with commits.
//!
//! Deletes are physical fjall removes, unversioned and unsynced: losing them
//! in a crash only leaves garbage for the next pass.

use std::ops::Bound;

use super::Engine;
use super::stored::{StoredValue, decode_stored};
use crate::encoding::split_versioned_key;
use crate::{Result, Timestamp};

/// Outcome of one GC chunk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GcChunk {
    /// Versions looked at.
    pub(crate) examined: u64,
    /// Versions removed.
    pub(crate) removed: u64,
    /// The first versioned key of the next chunk, or `None` at the end of `data`.
    pub(crate) resume: Option<Vec<u8>>,
}

/// Collect garbage from `start` (a versioned key, inclusive) onwards.
///
/// Stops at the first key boundary after examining `max_versions`
/// versions, so one key's versions are always handled together.
pub(crate) fn collect_chunk(
    engine: &Engine,
    start: Option<&[u8]>,
    watermark: Timestamp,
    max_versions: usize,
) -> Result<GcChunk> {
    let lower = start.map_or(Bound::Unbounded, |s| Bound::Included(s.to_vec()));
    let mut chunk = GcChunk::default();
    let mut removals = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    let mut shadowed = false;
    for guard in engine.data().range::<Vec<u8>, _>((lower, Bound::Unbounded)) {
        let (versioned, record) = guard.into_inner()?;
        let (tuple, ts) = split_versioned_key(&versioned)?;
        if current.as_deref() != Some(tuple) {
            if current.is_some() && chunk.examined >= max_versions as u64 {
                chunk.resume = Some(versioned.to_vec());
                break;
            }
            current = Some(tuple.to_vec());
            shadowed = false;
        }
        chunk.examined += 1;
        if shadowed {
            removals.push(versioned.to_vec());
        } else if ts <= watermark {
            shadowed = true;
            if matches!(decode_stored(record.into())?, StoredValue::Tombstone) {
                removals.push(versioned.to_vec());
            }
        }
    }
    chunk.removed = removals.len() as u64;
    engine.remove_versions(removals)?;
    Ok(chunk)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Return every `(tuple, ts)` left in `data`.
    fn versions(engine: &Engine) -> Vec<(Vec<u8>, u64)> {
        engine
            .data()
            .iter()
            .map(|g| {
                let (k, _) = g.into_inner().unwrap();
                let (tuple, ts) = split_versioned_key(&k).unwrap();
                (tuple.to_vec(), ts.into_raw())
            })
            .collect()
    }

    /// Commit one write of `key` at `ts` (`None` deletes).
    fn write(engine: &Engine, ts: u64, key: &[u8], value: Option<&[u8]>) {
        engine
            .commit_group(
                Timestamp::from_raw(ts),
                [(Timestamp::from_raw(ts), key, value)],
                &[],
                crate::Durability::Buffered,
            )
            .unwrap();
    }

    /// Run whole passes of small chunks at `watermark`; return totals.
    fn pass(engine: &Engine, watermark: u64) -> GcChunk {
        let mut total = GcChunk::default();
        let mut start = None;
        loop {
            let chunk =
                collect_chunk(engine, start.as_deref(), Timestamp::from_raw(watermark), 2).unwrap();
            total.examined += chunk.examined;
            total.removed += chunk.removed;
            match chunk.resume {
                Some(resume) => start = Some(resume),
                None => return total,
            }
        }
    }

    #[test]
    /// Assert only versions shadowed at the watermark, and tombstones at its edge, are removed.
    fn removes_exactly_the_obsolete_versions() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        // a: 1, 3, 5. b: 2, deleted at 4. c: 6 only. d: deleted at 7.
        write(&engine, 1, b"a", Some(b"1"));
        write(&engine, 2, b"b", Some(b"2"));
        write(&engine, 3, b"a", Some(b"3"));
        write(&engine, 4, b"b", None);
        write(&engine, 5, b"a", Some(b"5"));
        write(&engine, 6, b"c", Some(b"6"));
        write(&engine, 7, b"d", None);

        let report = pass(&engine, 3);
        assert_eq!(report.removed, 1, "only a@1 is shadowed at 3");
        assert_eq!(
            versions(&engine),
            vec![
                (b"a".to_vec(), 5),
                (b"a".to_vec(), 3),
                (b"b".to_vec(), 4),
                (b"b".to_vec(), 2),
                (b"c".to_vec(), 6),
                (b"d".to_vec(), 7),
            ]
        );

        let report = pass(&engine, 7);
        assert_eq!(report.examined, 6);
        assert_eq!(
            report.removed, 4,
            "a@3, b@4 (tombstone), b@2, d@7 (tombstone)"
        );
        assert_eq!(
            versions(&engine),
            vec![(b"a".to_vec(), 5), (b"c".to_vec(), 6)]
        );
        assert_eq!(pass(&engine, 7).removed, 0, "a second pass finds nothing");
    }
}
