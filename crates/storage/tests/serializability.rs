//! Concurrent randomized workloads checked against a serial replay (ADR-0009, milestone M3).
//!
//! Many tasks on a multi-threaded runtime run read-write register
//! transactions over a few keys, so commits contend, conflict and share
//! commit groups. Every write stores a unique value, so each read
//! identifies the write it observed. Afterwards the checker replays the
//! committed transactions in commit-timestamp order, the serial order
//! backward validation promises (ADR-0004), and checks:
//!
//! - every read and scan of a SERIALIZABLE transaction saw exactly the state
//!   left by the commits before it (no G0, G1, G-single, G2-item or phantoms);
//! - every read-only transaction saw exactly the state at its snapshot (no
//!   fractured reads).
//!
//! The negative test runs the same workload under REPEATABLE READ and
//! requires the checker to find a violation (write skew), proving the
//! harness can detect anomalies.

#[allow(dead_code, unused_imports)]
mod support;

use std::collections::BTreeMap;
use std::ops::Bound;

use bytes::Bytes;
use interlaken_storage::{
    Durability, Error, FjallStorage, Isolation, Key, KeyRange, ScanRequest, Storage,
    StorageOptions, TxnOptions,
};
use support::key;

/// Number of registers; small, so transactions contend.
const KEYS: u8 = 5;

/// Concurrent workers.
const WORKERS: u64 = 8;

/// The value every register starts with.
const INITIAL: u64 = 0;

/// One scan: the inclusive register range and the `(register, value)` rows seen.
type ScanObservation = ((u8, u8), Vec<(u8, u64)>);

/// What one committed transaction observed and wrote.
#[derive(Debug)]
struct Record {
    /// Commit timestamp, or the snapshot for a read-only transaction.
    ts: u64,
    read_only: bool,
    /// Point reads: register and the value seen (`None` if absent).
    reads: Vec<(u8, Option<u64>)>,
    /// Scans and the rows they returned.
    scans: Vec<ScanObservation>,
    /// Registers written and their new values.
    writes: Vec<(u8, u64)>,
}

/// A small deterministic generator, so each worker's choices are reproducible.
struct Rng(u64);

impl Rng {
    /// Return the next pseudo-random number (xorshift64*).
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Return a register index.
    fn register(&mut self) -> u8 {
        u8::try_from(self.next() % u64::from(KEYS)).unwrap()
    }
}

/// Decode a register index from its encoded key.
fn register_of(encoded: &[u8]) -> u8 {
    (0..KEYS).find(|&i| key(i) == encoded).unwrap()
}

/// Decode a stored value.
fn value_of(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().unwrap())
}

/// Run one worker's transactions; return the committed ones and the abort count.
async fn worker(
    storage: FjallStorage,
    id: u64,
    txns: u64,
    isolation: Isolation,
) -> (Vec<Record>, u64) {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (id + 1).wrapping_mul(0xD1B5_4A32_D192_ED03));
    let mut records = Vec::new();
    let mut aborts = 0;
    for seq in 0..txns {
        let read_only = rng.next().is_multiple_of(5);
        let options = if read_only {
            TxnOptions::new(isolation).read_only()
        } else {
            TxnOptions::new(isolation)
        };
        let txn = storage.begin(options).await.unwrap();
        let mut record = Record {
            ts: txn.snapshot().into_raw(),
            read_only,
            reads: Vec::new(),
            scans: Vec::new(),
            writes: Vec::new(),
        };

        if read_only || rng.next().is_multiple_of(3) {
            let (a, b) = (rng.register(), rng.register());
            let (lo, hi) = (a.min(b), a.max(b));
            let range = KeyRange::new(
                Bound::Included(Bytes::from(key(lo))),
                Bound::Included(Bytes::from(key(hi))),
            );
            let mut request = ScanRequest::new(range);
            request.limit_rows = 2;
            let mut rows = Vec::new();
            loop {
                let batch = txn.scan(request.clone()).await.unwrap();
                rows.extend(
                    batch
                        .rows
                        .iter()
                        .map(|(k, v)| (register_of(k), value_of(v))),
                );
                match request.resume_after(&batch) {
                    Some(next) => request = next,
                    None => break,
                }
            }
            record.scans.push(((lo, hi), rows));
        }
        let read_keys: Vec<u8> = (0..1 + rng.next() % 2).map(|_| rng.register()).collect();
        let values = txn
            .get_many(read_keys.iter().map(|&i| Key::from(key(i))).collect())
            .await
            .unwrap();
        record.reads = read_keys
            .iter()
            .zip(values)
            .map(|(&i, v)| (i, v.map(|b| value_of(&b))))
            .collect();

        if read_only {
            txn.commit().await.unwrap();
            records.push(record);
            continue;
        }
        let mut written = Vec::new();
        for _ in 0..1 + rng.next() % 2 {
            let register = rng.register();
            if written.contains(&register) {
                continue;
            }
            written.push(register);
            let value = (id << 32) | (seq + 1);
            txn.put(
                Bytes::from(key(register)),
                Bytes::from(value.to_be_bytes().to_vec()),
            )
            .unwrap();
            record.writes.push((register, value));
        }
        match txn.commit().await {
            Ok(info) => {
                record.ts = info.commit_ts.unwrap().into_raw();
                records.push(record);
            }
            Err(Error::SerializationFailure(_)) => aborts += 1,
            Err(other) => panic!("unexpected commit error: {other}"),
        }
    }
    (records, aborts)
}

/// Run the workload and return every committed record and the abort count.
async fn run_workload(
    isolation: Isolation,
    durability: Durability,
    txns: u64,
) -> (Vec<Record>, u64) {
    let dir = tempfile::tempdir().unwrap();
    let options = StorageOptions {
        durability,
        max_commit_group: 16,
        ..StorageOptions::default()
    };
    let storage = FjallStorage::open(dir.path(), options).unwrap();
    let seed = storage.begin(TxnOptions::default()).await.unwrap();
    for i in 0..KEYS {
        seed.put(
            Bytes::from(key(i)),
            Bytes::from(INITIAL.to_be_bytes().to_vec()),
        )
        .unwrap();
    }
    assert_eq!(
        seed.commit().await.unwrap().commit_ts.unwrap().into_raw(),
        1
    );

    let tasks: Vec<_> = (0..WORKERS)
        .map(|id| tokio::spawn(worker(storage.clone(), id, txns, isolation)))
        .collect();
    let mut records = Vec::new();
    let mut aborts = 0;
    for task in tasks {
        let (mut r, a) = task.await.unwrap();
        records.append(&mut r);
        aborts += a;
    }
    (records, aborts)
}

/// Replay `records` serially in timestamp order; return a description of each violation.
fn check(records: &[Record]) -> Vec<String> {
    let mut writers: Vec<&Record> = records.iter().filter(|r| !r.read_only).collect();
    writers.sort_by_key(|r| r.ts);
    for pair in writers.windows(2) {
        assert!(pair[0].ts < pair[1].ts, "duplicate commit timestamp");
    }

    // State after each commit, so read-only snapshots can be checked too.
    let mut state: BTreeMap<u8, u64> = (0..KEYS).map(|i| (i, INITIAL)).collect();
    let mut states: BTreeMap<u64, BTreeMap<u8, u64>> = BTreeMap::from([(1, state.clone())]);
    let mut violations = Vec::new();
    for record in &writers {
        violations.extend(observation_errors(record, &state, "before its commit"));
        for &(register, value) in &record.writes {
            state.insert(register, value);
        }
        states.insert(record.ts, state.clone());
    }
    for record in records.iter().filter(|r| r.read_only) {
        let (_, at_snapshot) = states
            .range(..=record.ts)
            .next_back()
            .expect("snapshots start after the seed commit");
        violations.extend(observation_errors(record, at_snapshot, "at its snapshot"));
    }
    violations
}

/// Compare a record's reads and scans with `state`; return each mismatch.
fn observation_errors(record: &Record, state: &BTreeMap<u8, u64>, when: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for &(register, seen) in &record.reads {
        let expected = state.get(&register).copied();
        if seen != expected {
            errors.push(format!(
                "txn ts {} read register {register} = {seen:?}, state {when} was {expected:?}",
                record.ts
            ));
        }
    }
    for ((lo, hi), rows) in &record.scans {
        let expected: Vec<(u8, u64)> = state.range(lo..=hi).map(|(&k, &v)| (k, v)).collect();
        if *rows != expected {
            errors.push(format!(
                "txn ts {} scanned {lo}..={hi} = {rows:?}, state {when} was {expected:?}",
                record.ts
            ));
        }
    }
    errors
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert SERIALIZABLE histories replay serially in commit order, with buffered durability.
async fn serializable_histories_replay_serially() {
    let (records, aborts) = run_workload(Isolation::Serializable, Durability::Buffered, 300).await;
    let violations = check(&records);
    assert!(
        violations.is_empty(),
        "anomalies found:\n{}",
        violations.join("\n")
    );
    let committed = records.iter().filter(|r| !r.read_only).count();
    assert!(
        committed > 100,
        "too few commits ({committed}) to be meaningful"
    );
    assert!(aborts > 0, "workload produced no conflicts");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert SERIALIZABLE histories replay serially with synced group commits.
async fn serializable_histories_replay_serially_with_sync() {
    let (records, _) = run_workload(Isolation::Serializable, Durability::Sync, 60).await;
    let violations = check(&records);
    assert!(
        violations.is_empty(),
        "anomalies found:\n{}",
        violations.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert the checker finds anomalies under REPEATABLE READ (the negative test).
async fn checker_detects_write_skew_under_repeatable_read() {
    for _ in 0..20 {
        let (records, _) = run_workload(Isolation::RepeatableRead, Durability::Buffered, 300).await;
        if !check(&records).is_empty() {
            return;
        }
    }
    panic!("checker found no anomaly under REPEATABLE READ in 20 runs");
}
