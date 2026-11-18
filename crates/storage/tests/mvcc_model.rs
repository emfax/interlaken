//! Model-based tests for MVCC reads, scans and the write buffer (ADR-0009, milestone M2).
//!
//! Random scripts run write transactions one at a time while read-only
//! "readers" stay open at older snapshots. Every read is compared against an
//! in-memory reference model with serial semantics: a map from key to
//! committed versions, plus a per-transaction write log tagged by statement.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::Arc;

use bytes::Bytes;
use interlaken_storage::encoding::{Datum, SortOrder, encode_tuple};
use interlaken_storage::{
    FjallStorage, Isolation, Key, KeyRange, RowFilter, ScanRequest, Storage, StorageOptions,
    Timestamp, Txn, TxnOptions,
};
use proptest::prelude::*;

/// Number of distinct keys in the universe.
const KEYS: u8 = 10;

/// Encode key index `i` as a one-column tuple, so all keys share one arity.
fn key(i: u8) -> Vec<u8> {
    encode_tuple(&[(Datum::Int8(i64::from(i)), SortOrder::ASC)]).unwrap()
}

/// A bound over key indices.
#[derive(Clone, Copy, Debug)]
enum BoundSpec {
    Unbounded,
    Included(u8),
    Excluded(u8),
}

impl BoundSpec {
    /// Convert to a bound over encoded keys.
    fn to_bound(self) -> Bound<Vec<u8>> {
        match self {
            Self::Unbounded => Bound::Unbounded,
            Self::Included(i) => Bound::Included(key(i)),
            Self::Excluded(i) => Bound::Excluded(key(i)),
        }
    }
}

/// Parameters of one scan, driven to completion through resume keys.
#[derive(Clone, Debug)]
struct ScanSpec {
    start: BoundSpec,
    end: BoundSpec,
    reverse: bool,
    limit_rows: usize,
    limit_bytes: usize,
    parity: Option<u8>,
}

/// One operation inside a transaction.
#[derive(Clone, Debug)]
enum Op {
    Put(u8, Vec<u8>),
    Delete(u8),
    Get(Vec<u8>),
    Scan(ScanSpec),
    NextStatement,
}

/// A write transaction run start to finish.
#[derive(Clone, Debug)]
struct TxnScript {
    isolation: Isolation,
    read_only: bool,
    ops: Vec<Op>,
    commit: bool,
}

/// One step of a test script.
#[derive(Clone, Debug)]
enum Step {
    /// Run a transaction to completion.
    Txn(TxnScript),
    /// Open a read-only transaction and keep it for later reads.
    OpenReader(Isolation),
    /// Run a read (or statement boundary) in a previously opened reader.
    ReaderOp(usize, Op),
}

/// Keep rows whose value's first byte has the given parity.
struct ParityFilter(u8);

impl RowFilter for ParityFilter {
    /// Keep the row if its value starts with a byte of the configured parity.
    fn eval(&self, _key: &[u8], value: &[u8]) -> bool {
        parity_matches(self.0, value)
    }
}

/// Return `true` if `value`'s first byte has parity `parity`.
fn parity_matches(parity: u8, value: &[u8]) -> bool {
    value.first().is_some_and(|b| b % 2 == parity)
}

/// Versions of one key in order: `(timestamp or statement, value)`, `None` for a delete.
type VersionLog = Vec<(u64, Option<Vec<u8>>)>;

/// The reference store: committed versions per key, in commit order.
#[derive(Default)]
struct Model {
    versions: BTreeMap<Vec<u8>, VersionLog>,
    last: u64,
}

impl Model {
    /// Return the value of `key` visible at `ts`.
    fn read_at(&self, key: &[u8], ts: u64) -> Option<Vec<u8>> {
        self.versions
            .get(key)?
            .iter()
            .rev()
            .find(|(t, _)| *t <= ts)
            .and_then(|(_, v)| v.clone())
    }
}

/// The reference transaction: snapshot, statement counter and write log.
struct ModelTxn {
    isolation: Isolation,
    snapshot: u64,
    statement: u64,
    writes: BTreeMap<Vec<u8>, VersionLog>,
}

impl ModelTxn {
    /// Begin at the model's latest commit.
    fn begin(model: &Model, isolation: Isolation) -> Self {
        Self {
            isolation,
            snapshot: model.last,
            statement: 0,
            writes: BTreeMap::new(),
        }
    }

    /// Return the value of `key` this transaction reads.
    fn read(&self, model: &Model, key: &[u8]) -> Option<Vec<u8>> {
        if let Some(log) = self.writes.get(key)
            && let Some((_, value)) = log.iter().rev().find(|(s, _)| *s < self.statement)
        {
            return value.clone();
        }
        model.read_at(key, self.snapshot)
    }

    /// Return every row of a scan, in scan order.
    fn scan(&self, model: &Model, spec: &ScanSpec) -> Vec<(Vec<u8>, Vec<u8>)> {
        let range = (spec.start.to_bound(), spec.end.to_bound());
        let keys: BTreeSet<&Vec<u8>> = model.versions.keys().chain(self.writes.keys()).collect();
        let mut rows: Vec<_> = keys
            .into_iter()
            .filter(|k| std::ops::RangeBounds::contains(&range, *k))
            .filter_map(|k| self.read(model, k).map(|v| (k.clone(), v)))
            .filter(|(_, v)| spec.parity.is_none_or(|p| parity_matches(p, v)))
            .collect();
        if spec.reverse {
            rows.reverse();
        }
        rows
    }

    /// Record a write in the current statement.
    fn write(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) {
        self.writes
            .entry(key)
            .or_default()
            .push((self.statement, value));
    }

    /// Advance the statement, refreshing the snapshot under READ COMMITTED.
    fn next_statement(&mut self, model: &Model) {
        self.statement += 1;
        if self.isolation == Isolation::ReadCommitted {
            self.snapshot = model.last;
        }
    }

    /// Apply the final write to each key as one commit; return its timestamp.
    fn commit(self, model: &mut Model) -> Option<u64> {
        if self.writes.is_empty() {
            return None;
        }
        model.last += 1;
        for (key, log) in self.writes {
            let value = log.last().expect("logs are never empty").1.clone();
            model
                .versions
                .entry(key)
                .or_default()
                .push((model.last, value));
        }
        Some(model.last)
    }
}

/// Generate an isolation level.
fn isolation() -> impl Strategy<Value = Isolation> {
    prop_oneof![
        Just(Isolation::ReadCommitted),
        Just(Isolation::RepeatableRead),
        Just(Isolation::Serializable),
    ]
}

/// Generate a bound over key indices, slightly beyond the universe at both ends.
fn bound_spec() -> impl Strategy<Value = BoundSpec> {
    prop_oneof![
        1 => Just(BoundSpec::Unbounded),
        2 => (0..=KEYS).prop_map(BoundSpec::Included),
        2 => (0..=KEYS).prop_map(BoundSpec::Excluded),
    ]
}

/// Generate scan parameters with small limits so scans span many batches.
fn scan_spec() -> impl Strategy<Value = ScanSpec> {
    (
        bound_spec(),
        bound_spec(),
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

/// Generate a read-side operation.
fn read_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        prop::collection::vec(0..KEYS, 1..4).prop_map(Op::Get),
        scan_spec().prop_map(Op::Scan),
        Just(Op::NextStatement),
    ]
}

/// Generate any operation.
fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..KEYS, prop::collection::vec(any::<u8>(), 0..3)).prop_map(|(k, v)| Op::Put(k, v)),
        1 => (0..KEYS).prop_map(Op::Delete),
        3 => read_op(),
    ]
}

/// Generate one script step.
fn step() -> impl Strategy<Value = Step> {
    let txn = (
        isolation(),
        prop::bool::weighted(0.15),
        prop::collection::vec(op(), 0..10),
        prop::bool::weighted(0.85),
    )
        .prop_map(|(isolation, read_only, ops, commit)| TxnScript {
            isolation,
            read_only,
            ops,
            commit,
        });
    prop_oneof![
        4 => txn.prop_map(Step::Txn),
        1 => isolation().prop_map(Step::OpenReader),
        2 => (any::<usize>(), read_op()).prop_map(|(i, op)| Step::ReaderOp(i, op)),
    ]
}

/// Run one scan to completion and check every batch's invariants.
async fn run_scan(txn: &Txn, spec: &ScanSpec) -> Vec<(Vec<u8>, Vec<u8>)> {
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

/// Run a read-side operation against both the storage and the model and compare.
async fn run_read(txn: &Txn, model_txn: &mut ModelTxn, model: &Model, op: &Op) {
    match op {
        Op::Get(indices) => {
            let keys: Vec<Key> = indices.iter().map(|i| Bytes::from(key(*i))).collect();
            let actual: Vec<Option<Vec<u8>>> = txn
                .get_many(keys.clone())
                .await
                .unwrap()
                .into_iter()
                .map(|v| v.map(|b| b.to_vec()))
                .collect();
            let expected: Vec<_> = keys.iter().map(|k| model_txn.read(model, k)).collect();
            assert_eq!(actual, expected, "get_many {indices:?}");
        }
        Op::Scan(spec) => {
            assert_eq!(
                run_scan(txn, spec).await,
                model_txn.scan(model, spec),
                "scan {spec:?}"
            );
        }
        Op::NextStatement => {
            txn.next_statement();
            model_txn.next_statement(model);
        }
        Op::Put(..) | Op::Delete(_) => unreachable!("not a read"),
    }
}

/// Run a whole script against fresh storage and the model.
async fn run_script(steps: Vec<Step>) {
    let dir = tempfile::tempdir().unwrap();
    let options = StorageOptions {
        pool_threads: 2,
        queue_capacity: 8,
    };
    let storage = FjallStorage::open(dir.path(), options).unwrap();
    let mut model = Model::default();
    let mut readers: Vec<(Txn, ModelTxn)> = Vec::new();

    for step in steps {
        match step {
            Step::Txn(script) => {
                let options = TxnOptions {
                    isolation: script.isolation,
                    read_only: script.read_only,
                };
                let txn = storage.begin(options).await.unwrap();
                let mut model_txn = ModelTxn::begin(&model, script.isolation);
                for op in &script.ops {
                    match op {
                        Op::Put(i, v) => {
                            let result = txn.put(Bytes::from(key(*i)), Bytes::from(v.clone()));
                            assert_eq!(result.is_ok(), !script.read_only);
                            if !script.read_only {
                                model_txn.write(key(*i), Some(v.clone()));
                            }
                        }
                        Op::Delete(i) => {
                            assert_eq!(txn.delete(Bytes::from(key(*i))).is_ok(), !script.read_only);
                            if !script.read_only {
                                model_txn.write(key(*i), None);
                            }
                        }
                        read => run_read(&txn, &mut model_txn, &model, read).await,
                    }
                }
                if script.commit {
                    let info = txn.commit().await.unwrap();
                    let expected = model_txn.commit(&mut model);
                    assert_eq!(info.commit_ts.map(Timestamp::into_raw), expected);
                } else {
                    txn.abort();
                }
                assert_eq!(storage.visible_ts().into_raw(), model.last);
            }
            Step::OpenReader(isolation) => {
                let txn = storage
                    .begin(TxnOptions::new(isolation).read_only())
                    .await
                    .unwrap();
                readers.push((txn, ModelTxn::begin(&model, isolation)));
            }
            Step::ReaderOp(i, op) => {
                if readers.is_empty() {
                    continue;
                }
                let n = readers.len();
                let (txn, model_txn) = &mut readers[i % n];
                run_read(txn, model_txn, &model, &op).await;
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Reads, scans and commits match the reference model.
    #[test]
    fn storage_matches_model(steps in prop::collection::vec(step(), 1..16)) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_script(steps));
    }
}
