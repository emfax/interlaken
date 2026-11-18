//! Model-based tests for MVCC reads, scans and the write buffer (ADR-0009, milestone M2).
//!
//! Random scripts run write transactions one at a time while read-only
//! "readers" stay open at older snapshots. Every read is compared against an
//! in-memory reference model with serial semantics (see `support::model`).

#[allow(dead_code, unused_imports)]
mod support;

use bytes::Bytes;
use interlaken_storage::{
    Durability, FjallStorage, Isolation, Key, Storage, StorageOptions, Timestamp, Txn, TxnOptions,
};
use proptest::prelude::*;
use support::{Model, ModelTxn, ScanSpec, key, run_scan, scan_spec};

/// Number of distinct keys in the universe.
const KEYS: u8 = 10;

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

/// Generate an isolation level.
fn isolation() -> impl Strategy<Value = Isolation> {
    prop_oneof![
        Just(Isolation::ReadCommitted),
        Just(Isolation::RepeatableRead),
        Just(Isolation::Serializable),
    ]
}

/// Generate a read-side operation.
fn read_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        prop::collection::vec(0..KEYS, 1..4).prop_map(Op::Get),
        scan_spec(KEYS).prop_map(Op::Scan),
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

/// Run a read-side operation against both the storage and the model and compare.
async fn run_read(txn: &Txn, model_txn: &mut ModelTxn, model: &Model, op: &Op) {
    match op {
        Op::Get(indices) => {
            let keys: Vec<Vec<u8>> = indices.iter().map(|i| key(*i)).collect();
            let actual: Vec<Option<Vec<u8>>> = txn
                .get_many(keys.iter().cloned().map(Key::from).collect())
                .await
                .unwrap()
                .into_iter()
                .map(|v| v.map(|b| b.to_vec()))
                .collect();
            assert_eq!(actual, model_txn.get(model, &keys), "get_many {indices:?}");
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
        durability: Durability::Buffered,
        ..StorageOptions::default()
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
                let mut model_txn = ModelTxn::begin(&model, script.isolation, script.read_only);
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
                    // Transactions run one at a time, so validation never fails.
                    let info = txn.commit().await.unwrap();
                    let expected = model_txn.commit(&mut model).unwrap();
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
                readers.push((txn, ModelTxn::begin(&model, isolation, true)));
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
