//! Keys, scan specifications and a scan driver that checks per-batch invariants.

use std::ops::Bound;
use std::sync::Arc;

use bytes::Bytes;
use interlaken_storage::encoding::{Datum, SortOrder, encode_tuple};
use interlaken_storage::{KeyRange, RowFilter, ScanRequest, Txn};
use proptest::prelude::*;

/// Encode key index `i` as a one-column tuple, so all keys share one arity.
pub fn key(i: u8) -> Vec<u8> {
    encode_tuple(&[(Datum::Int8(i64::from(i)), SortOrder::ASC)]).unwrap()
}

/// A bound over key indices.
#[derive(Clone, Copy, Debug)]
pub enum BoundSpec {
    Unbounded,
    Included(u8),
    Excluded(u8),
}

impl BoundSpec {
    /// Convert to a bound over encoded keys.
    pub fn to_bound(self) -> Bound<Vec<u8>> {
        match self {
            Self::Unbounded => Bound::Unbounded,
            Self::Included(i) => Bound::Included(key(i)),
            Self::Excluded(i) => Bound::Excluded(key(i)),
        }
    }
}

/// Parameters of one scan, driven to completion through resume keys.
#[derive(Clone, Debug)]
pub struct ScanSpec {
    pub start: BoundSpec,
    pub end: BoundSpec,
    pub reverse: bool,
    pub limit_rows: usize,
    pub limit_bytes: usize,
    pub parity: Option<u8>,
}

impl ScanSpec {
    /// Return the scanned range over encoded keys.
    pub fn bounds(&self) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
        (self.start.to_bound(), self.end.to_bound())
    }
}

/// Keep rows whose value's first byte has the given parity.
pub struct ParityFilter(pub u8);

impl ParityFilter {
    /// Return `true` if `value`'s first byte has this filter's parity.
    pub fn keeps(&self, value: &[u8]) -> bool {
        value.first().is_some_and(|b| b % 2 == self.0)
    }
}

impl RowFilter for ParityFilter {
    /// Keep the row if its value starts with a byte of the configured parity.
    fn eval(&self, _key: &[u8], value: &[u8]) -> bool {
        self.keeps(value)
    }
}

/// Generate a bound over key indices `0..keys`, slightly beyond the universe at both ends.
fn bound_spec(keys: u8) -> impl Strategy<Value = BoundSpec> {
    prop_oneof![
        1 => Just(BoundSpec::Unbounded),
        2 => (0..=keys).prop_map(BoundSpec::Included),
        2 => (0..=keys).prop_map(BoundSpec::Excluded),
    ]
}

/// Generate scan parameters with small limits so scans span many batches.
pub fn scan_spec(keys: u8) -> impl Strategy<Value = ScanSpec> {
    (
        bound_spec(keys),
        bound_spec(keys),
        any::<bool>(),
        1usize..4,
        1usize..40,
        prop::option::of(0u8..2),
    )
        .prop_map(
            |(start, end, reverse, limit_rows, limit_bytes, parity)| ScanSpec {
                start,
                end,
                reverse,
                limit_rows,
                limit_bytes,
                parity,
            },
        )
}

/// Run one scan to completion, check every batch's invariants and return all rows.
pub async fn run_scan(txn: &Txn, spec: &ScanSpec) -> Vec<(Vec<u8>, Vec<u8>)> {
    let range = KeyRange::new(
        spec.start.to_bound().map(Bytes::from),
        spec.end.to_bound().map(Bytes::from),
    );
    let mut request = ScanRequest::new(range.clone());
    request.reverse = spec.reverse;
    request.limit_rows = spec.limit_rows;
    request.limit_bytes = spec.limit_bytes;
    request.filter = spec
        .parity
        .map(|p| Arc::new(ParityFilter(p)) as Arc<dyn RowFilter>);

    let mut rows = Vec::new();
    for _ in 0..1000 {
        let batch = txn.scan(request.clone()).await.unwrap();
        assert!(
            batch.rows.len() <= spec.limit_rows,
            "batch exceeds row limit"
        );
        let sizes: Vec<usize> = batch.rows.iter().map(|(k, v)| k.len() + v.len()).collect();
        if let Some((_, before_last)) = sizes.split_last() {
            assert!(
                before_last.iter().sum::<usize>() < spec.limit_bytes,
                "batch kept going past byte limit"
            );
        }
        for (k, _) in &batch.rows {
            assert!(range.contains(k), "row outside range");
        }
        if let (Some(resume), Some((last, _))) = (&batch.resume, batch.rows.last()) {
            assert!(
                if spec.reverse {
                    resume <= last
                } else {
                    resume >= last
                },
                "resume behind last row"
            );
        }
        rows.extend(batch.rows.iter().map(|(k, v)| (k.to_vec(), v.to_vec())));
        match request.resume_after(&batch) {
            Some(next) => request = next,
            None => return rows,
        }
    }
    panic!("scan did not terminate");
}
