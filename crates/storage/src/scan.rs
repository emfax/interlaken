//! Public types for batched reads: keys, key ranges, scan requests and filters (ADR-0006).

use std::fmt;
use std::ops::Bound;
use std::sync::Arc;

use bytes::Bytes;

/// An encoded key tuple (see [`encoding`](crate::encoding)), without a version suffix.
pub type Key = Bytes;

/// Default maximum rows per [`ScanBatch`].
pub const DEFAULT_SCAN_LIMIT_ROWS: usize = 1024;

/// Default maximum bytes (keys plus values) per [`ScanBatch`].
pub const DEFAULT_SCAN_LIMIT_BYTES: usize = 1024 * 1024;

/// A range of keys, compared bytewise.
///
/// Bounds are encoded tuples. A bound may be a full key or a prefix of the
/// keys under one `(table_id, index_id)`, but must not be longer than those
/// keys (see the arity rule in [`encoding`](crate::encoding)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRange {
    /// Lower bound.
    pub start: Bound<Key>,
    /// Upper bound.
    pub end: Bound<Key>,
}

impl KeyRange {
    /// Construct a range from explicit bounds.
    #[must_use]
    pub fn new(start: Bound<Key>, end: Bound<Key>) -> Self {
        Self { start, end }
    }

    /// Construct the range covering every key.
    #[must_use]
    pub fn all() -> Self {
        Self::new(Bound::Unbounded, Bound::Unbounded)
    }

    /// Construct the range covering a single key.
    #[must_use]
    pub fn point(key: Key) -> Self {
        Self::new(Bound::Included(key.clone()), Bound::Included(key))
    }

    /// Construct the range of keys that start with `prefix`.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_storage::KeyRange;
    ///
    /// let range = KeyRange::prefix(b"ab");
    /// assert!(range.contains(b"abc"));
    /// assert!(!range.contains(b"ac"));
    /// ```
    #[must_use]
    pub fn prefix(prefix: &[u8]) -> Self {
        let start = Bound::Included(Bytes::copy_from_slice(prefix));
        let end = match prefix.iter().rposition(|&b| b != 0xFF) {
            Some(i) => {
                let mut upper = prefix[..=i].to_vec();
                upper[i] += 1;
                Bound::Excluded(Bytes::from(upper))
            }
            None => Bound::Unbounded,
        };
        Self::new(start, end)
    }

    /// Return `true` if `key` lies within the range.
    #[must_use]
    pub fn contains(&self, key: &[u8]) -> bool {
        self.after_start(key) && self.before_end(key)
    }

    /// Return `true` if no key can lie within the range, because the bounds
    /// are inverted or meet without overlapping.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::ops::Bound;
    /// use bytes::Bytes;
    /// use interlaken_storage::KeyRange;
    ///
    /// let b = Bytes::from_static(b"b");
    /// assert!(KeyRange::new(Bound::Included(b.clone()), Bound::Excluded(b.clone())).is_empty());
    /// assert!(!KeyRange::point(b).is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match (&self.start, &self.end) {
            (Bound::Included(s), Bound::Included(e)) => s > e,
            (Bound::Included(s) | Bound::Excluded(s), Bound::Included(e) | Bound::Excluded(e)) => {
                s >= e
            }
            _ => false,
        }
    }

    /// Return `true` if `key` satisfies the lower bound.
    pub(crate) fn after_start(&self, key: &[u8]) -> bool {
        match &self.start {
            Bound::Included(s) => key >= &s[..],
            Bound::Excluded(s) => key > &s[..],
            Bound::Unbounded => true,
        }
    }

    /// Return `true` if `key` satisfies the upper bound.
    pub(crate) fn before_end(&self, key: &[u8]) -> bool {
        match &self.end {
            Bound::Included(e) => key <= &e[..],
            Bound::Excluded(e) => key < &e[..],
            Bound::Unbounded => true,
        }
    }

    /// Return the bounds as borrowed slices, for `BTreeMap::range`.
    pub(crate) fn as_slice_bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        (as_slice_bound(&self.start), as_slice_bound(&self.end))
    }
}

/// Borrow a key bound as a slice bound.
fn as_slice_bound(bound: &Bound<Key>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(k) => Bound::Included(&k[..]),
        Bound::Excluded(k) => Bound::Excluded(&k[..]),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// A predicate evaluated on the storage thread during a scan.
///
/// Implementations must be pure: no side effects, no errors that depend on
/// the data, and the same answer for the same input. Rows a filter rejects
/// still enter the transaction's read set (ADR-0006).
pub trait RowFilter: Send + Sync {
    /// Evaluate the predicate on one row; return `true` to keep it.
    fn eval(&self, key: &[u8], value: &[u8]) -> bool;
}

/// One batched scan call.
///
/// Projection pushdown is deferred until the row format lands (milestone M4).
#[derive(Clone)]
pub struct ScanRequest {
    /// Keys to scan.
    pub range: KeyRange,
    /// Scan from the end of the range towards the start.
    pub reverse: bool,
    /// Maximum rows in the batch (at least one is always allowed).
    pub limit_rows: usize,
    /// Stop once the batch holds this many bytes of keys plus values.
    pub limit_bytes: usize,
    /// Resume key from the previous [`ScanBatch`] of the same scan.
    pub resume: Option<Key>,
    /// Predicate run on the storage thread.
    pub filter: Option<Arc<dyn RowFilter>>,
}

impl ScanRequest {
    /// Construct a forward scan over `range` with default limits.
    #[must_use]
    pub fn new(range: KeyRange) -> Self {
        Self {
            range,
            reverse: false,
            limit_rows: DEFAULT_SCAN_LIMIT_ROWS,
            limit_bytes: DEFAULT_SCAN_LIMIT_BYTES,
            resume: None,
            filter: None,
        }
    }

    /// Return the same request continuing after `batch`, or `None` if the range is exhausted.
    #[must_use]
    pub fn resume_after(&self, batch: &ScanBatch) -> Option<Self> {
        batch.resume.clone().map(|resume| Self {
            resume: Some(resume),
            ..self.clone()
        })
    }
}

impl fmt::Debug for ScanRequest {
    /// Render the request, showing only whether a filter is present.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScanRequest")
            .field("range", &self.range)
            .field("reverse", &self.reverse)
            .field("limit_rows", &self.limit_rows)
            .field("limit_bytes", &self.limit_bytes)
            .field("resume", &self.resume)
            .field("filter", &self.filter.as_ref().map(|_| "<filter>"))
            .finish()
    }
}

/// The result of one scan call.
///
/// A batch may hold fewer rows than the limit, or none at all, while
/// `resume` is `Some`: each call bounds the keys it examines so that highly
/// selective filters can't monopolise a storage thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanBatch {
    /// Rows in scan order.
    pub rows: Vec<(Key, Bytes)>,
    /// Where to continue; `None` when the range is exhausted.
    pub resume: Option<Key>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert prefix ranges handle trailing `0xFF` bytes.
    fn prefix_upper_bound() {
        assert_eq!(
            KeyRange::prefix(b"a\xff").end,
            Bound::Excluded(Bytes::from_static(b"b"))
        );
        assert_eq!(KeyRange::prefix(b"\xff\xff").end, Bound::Unbounded);
        assert!(KeyRange::prefix(b"").contains(b"anything"));
    }
}
