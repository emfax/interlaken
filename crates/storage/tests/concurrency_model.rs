//! Model-based tests with concurrent transactions (ADR-0009, milestone M3).
//!
//! Random scripts interleave the operations of several open transactions at
//! every isolation level, with savepoints, statement boundaries, commits and
//! aborts. Every read result and every commit outcome (timestamp or
//! conflict) is compared against the reference model in `support::model`,
//! which implements backward validation independently of the engine.

#[allow(dead_code, unused_imports)]
mod support;

use bytes::Bytes;
use interlaken_storage::{
    Durability, Error, FjallStorage, Isolation, Key, SavepointId, Storage, StorageOptions, Txn,
    TxnOptions,
};
use proptest::prelude::*;
use support::{Model, ModelTxn, ScanSpec, key, run_scan, scan_spec};

/// Number of distinct keys; kept small so transactions collide often.
const KEYS: u8 = 4;

/// Most transactions open at once.
const MAX_OPEN: usize = 3;

/// One operation on an open transaction.
#[derive(Clone, Debug)]
enum Op {
    Put(u8, u8),
    Delete(u8),
    Get(Vec<u8>),
    Scan(ScanSpec),
    NextStatement,
    Savepoint,
    RollbackTo(usize),
    Release(usize),
}

/// One step of a script. Indices pick among the open transactions, modulo
/// their number, so steps are rarely wasted.
#[derive(Clone, Debug)]
enum Step {
    /// Begin a transaction, unless `MAX_OPEN` are already open.
    Begin(Isolation, bool),
    /// Apply an operation to an open transaction.
    Op(usize, Op),
    /// Commit an open transaction.
    Commit(usize),
    /// Abort an open transaction.
    Abort(usize),
    /// Run a version GC pass, which must be invisible to every open transaction.
    Gc,
}

/// An open transaction with its model twin and savepoint handles.
struct Open {
    txn: Txn,
    model: ModelTxn,
    savepoints: Vec<SavepointId>,
}

/// Generate an isolation level, favouring SERIALIZABLE, which has the most checks.
fn isolation() -> impl Strategy<Value = Isolation> {
    prop_oneof![
        1 => Just(Isolation::ReadCommitted),
        1 => Just(Isolation::RepeatableRead),
        2 => Just(Isolation::Serializable),
    ]
}

/// Generate an operation.
fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0..KEYS, any::<u8>()).prop_map(|(k, v)| Op::Put(k, v)),
        1 => (0..KEYS).prop_map(Op::Delete),
        3 => prop::collection::vec(0..KEYS, 1..3).prop_map(Op::Get),
        2 => scan_spec(KEYS).prop_map(Op::Scan),
        4 => Just(Op::NextStatement),
        2 => Just(Op::Savepoint),
        2 => any::<usize>().prop_map(Op::RollbackTo),
        1 => any::<usize>().prop_map(Op::Release),
    ]
}

/// Generate a script step.
fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        2 => (isolation(), prop::bool::weighted(0.1))
            .prop_map(|(isolation, read_only)| Step::Begin(isolation, read_only)),
        12 => (any::<usize>(), op()).prop_map(|(i, op)| Step::Op(i, op)),
        2 => any::<usize>().prop_map(Step::Commit),
        1 => any::<usize>().prop_map(Step::Abort),
        1 => Just(Step::Gc),
    ]
}

/// Apply `op` to the open transaction and its model and compare results.
async fn apply(open: &mut Open, model: &Model, op: &Op) {
    let Open {
        txn,
        model: model_txn,
        savepoints,
    } = open;
    match op {
        Op::Put(i, v) => {
            let result = txn.put(Bytes::from(key(*i)), Bytes::from(vec![*v]));
            assert_eq!(result.is_ok(), !model_txn.read_only());
            if result.is_ok() {
                model_txn.write(key(*i), Some(vec![*v]));
            }
        }
        Op::Delete(i) => {
            let result = txn.delete(Bytes::from(key(*i)));
            assert_eq!(result.is_ok(), !model_txn.read_only());
            if result.is_ok() {
                model_txn.write(key(*i), None);
            }
        }
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
        Op::Savepoint => {
            savepoints.push(txn.savepoint());
            assert_eq!(model_txn.savepoint(), savepoints.len() - 1);
        }
        Op::RollbackTo(_) | Op::Release(_) if savepoints.is_empty() => {}
        Op::RollbackTo(i) => {
            let index = i % savepoints.len();
            txn.rollback_to(savepoints[index]).unwrap();
            model_txn.rollback_to(index);
            let released = savepoints.split_off(index + 1);
            for sp in released {
                assert!(matches!(txn.rollback_to(sp), Err(Error::UnknownSavepoint)));
            }
        }
        Op::Release(i) => {
            let index = i % savepoints.len();
            txn.release_savepoint(savepoints[index]).unwrap();
            model_txn.release(index);
            savepoints.truncate(index);
        }
    }
    assert_eq!(savepoints.len(), model_txn.open_savepoints());
}

/// Run a whole script against fresh storage and the model.
async fn run_script(steps: Vec<Step>) {
    let dir = tempfile::tempdir().unwrap();
    let options = StorageOptions {
        pool_threads: 2,
        queue_capacity: 8,
        durability: Durability::Buffered,
        gc_interval: None,
        gc_chunk_versions: 3,
        ..StorageOptions::default()
    };
    let storage = FjallStorage::open(dir.path(), options).unwrap();
    let mut model = Model::default();
    let mut open: Vec<Open> = Vec::new();

    for step in steps {
        match step {
            Step::Begin(isolation, read_only) => {
                if open.len() == MAX_OPEN {
                    continue;
                }
                let txn = storage
                    .begin(TxnOptions {
                        isolation,
                        read_only,
                        ..TxnOptions::default()
                    })
                    .await
                    .unwrap();
                open.push(Open {
                    txn,
                    model: ModelTxn::begin(&model, isolation, read_only),
                    savepoints: Vec::new(),
                });
            }
            Step::Gc => {
                storage.run_gc().await.unwrap();
            }
            Step::Op(_, _) | Step::Commit(_) | Step::Abort(_) if open.is_empty() => {}
            Step::Op(i, op) => {
                let n = open.len();
                apply(&mut open[i % n], &model, &op).await;
            }
            Step::Commit(i) => {
                let slot = i % open.len();
                let open = open.swap_remove(slot);
                let actual = match open.txn.commit().await {
                    Ok(info) => Ok(info.commit_ts.map(|ts| ts.into_raw())),
                    Err(Error::SerializationFailure(conflict)) => Err(conflict),
                    Err(other) => panic!("unexpected commit error: {other}"),
                };
                assert_eq!(actual, open.model.commit(&mut model), "commit slot {slot}");
                assert_eq!(storage.visible_ts().into_raw(), model.last);
            }
            Step::Abort(i) => {
                let slot = i % open.len();
                open.swap_remove(slot).txn.abort();
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Interleaved transactions' reads and commit outcomes match the reference model.
    #[test]
    fn interleavings_match_model(steps in prop::collection::vec(step(), 1..128)) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_script(steps));
    }
}
