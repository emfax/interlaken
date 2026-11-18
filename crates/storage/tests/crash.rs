//! Crash harness: kill the process mid-commit, recover, verify (ADR-0009 phase 6, ADR-0005).
//!
//! The parent test re-runs this test binary as a child that commits as fast
//! as it can with [`Durability::Sync`] while version GC runs every
//! millisecond. The child prints a line for each acknowledged commit, and
//! for each delete it is about to attempt. The parent kills it (`SIGKILL`)
//! at a random moment, reopens the database and checks:
//!
//! - every acknowledged commit is present, unless a later delete targeted it;
//! - every transaction is all-or-nothing: its four keys, and a delete
//!   transaction's removal of its victim, appear together or not at all;
//! - the recovered commit clock is at least every acknowledged timestamp,
//!   and the next run's commits are numbered after it.
//!
//! It then runs the next cycle on the same directory, so recovery itself is
//! crashed into repeatedly. `CRASH_CYCLES` sets the cycle count (default 6).
//!
//! `SIGKILL` loses no data the process already handed to the kernel, so
//! this checks crash consistency and recovery, not power loss: a commit
//! acknowledged under [`Durability::Buffered`] would also survive it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use interlaken_storage::{
    Durability, FjallStorage, Key, KeyRange, ScanRequest, Storage, StorageOptions, TxnOptions,
};

/// Keys each transaction writes.
const KEYS_PER_TXN: u64 = 4;

/// The keys transaction `id` writes.
fn txn_keys(id: u64) -> Vec<Key> {
    (0..KEYS_PER_TXN)
        .map(|k| Bytes::from(format!("t/{id:016}/{k}")))
        .collect()
}

/// Parse the transaction ID out of a key.
fn id_of(key: &[u8]) -> u64 {
    let text = std::str::from_utf8(key).unwrap();
    text[2..18].parse().unwrap()
}

/// What the child reported.
#[derive(Default)]
struct Report {
    /// Acknowledged commits: ID and commit timestamp.
    acked: BTreeMap<u64, u64>,
    /// Delete transactions attempted: delete ID to victim ID.
    deletes: BTreeMap<u64, u64>,
}

#[test]
#[ignore = "run by crash_recovery_loses_no_acknowledged_commits as a child process"]
/// Commit transactions forever, reporting acknowledgements on stdout.
fn crash_child() {
    let dir = std::env::var("CRASH_DIR").expect("CRASH_DIR");
    let base: u64 = std::env::var("CRASH_BASE").unwrap().parse().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let options = StorageOptions {
            durability: Durability::Sync,
            gc_interval: Some(Duration::from_millis(1)),
            gc_chunk_versions: 16,
            ..StorageOptions::default()
        };
        let storage = FjallStorage::open(&dir, options).unwrap();
        say(&format!("READY {}", storage.visible_ts().into_raw()));
        let next = Arc::new(AtomicU64::new(0));
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let storage = storage.clone();
                let next = next.clone();
                tokio::spawn(async move {
                    let mut mine: Vec<u64> = Vec::new();
                    loop {
                        let id = base + next.fetch_add(1, Ordering::Relaxed);
                        let txn = storage.begin(TxnOptions::default()).await.unwrap();
                        for key in txn_keys(id) {
                            txn.put(key, Bytes::from(id.to_be_bytes().to_vec()))
                                .unwrap();
                        }
                        // Every few transactions also delete one of this worker's earlier ones.
                        if id % 5 == worker % 5 && !mine.is_empty() {
                            let victim = mine.swap_remove((id as usize) % mine.len());
                            for key in txn_keys(victim) {
                                txn.delete(key).unwrap();
                            }
                            say(&format!("TRY {id} {victim}"));
                        }
                        if let Ok(info) = txn.commit().await {
                            say(&format!("ACK {id} {}", info.commit_ts.unwrap().into_raw()));
                            mine.push(id);
                        }
                    }
                })
            })
            .collect();
        // Workers run until the parent kills the process.
        let _workers = workers;
        std::future::pending::<()>().await;
    });
}

/// Print one line to stdout immediately, bypassing the test harness's capture.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").unwrap();
    out.flush().unwrap();
}

/// Run the child for a random time, kill it, and return what it reported.
fn run_child(dir: &std::path::Path, base: u64, kill_after: Duration) -> (u64, Report) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "crash_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("CRASH_DIR", dir)
        .env("CRASH_BASE", base.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (lines_tx, lines_rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });

    // Wait for the child to open the database, then let it run.
    let mut start_ts = None;
    while start_ts.is_none() {
        let line = lines_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("child did not start");
        // The harness prints "test crash_child ... " without a newline first.
        if let Some((_, ts)) = line.split_once("READY ") {
            start_ts = Some(ts.trim().parse().unwrap());
        }
    }
    std::thread::sleep(kill_after);
    child.kill().unwrap();
    child.wait().unwrap();
    reader.join().unwrap();

    let mut report = Report::default();
    for line in lines_rx.try_iter() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["ACK", id, ts] => {
                report
                    .acked
                    .insert(id.parse().unwrap(), ts.parse().unwrap());
            }
            ["TRY", id, victim] => {
                report
                    .deletes
                    .insert(id.parse().unwrap(), victim.parse().unwrap());
            }
            _ => {}
        }
    }
    (start_ts.unwrap(), report)
}

/// Reopen the database and return the visible timestamp and every key's transaction ID.
async fn recover(dir: &std::path::Path) -> (u64, BTreeMap<u64, usize>) {
    let options = StorageOptions {
        durability: Durability::Buffered,
        gc_interval: None,
        ..StorageOptions::default()
    };
    let storage = FjallStorage::open(dir, options).unwrap();
    let txn = storage
        .begin(TxnOptions::default().read_only())
        .await
        .unwrap();
    let mut present: BTreeMap<u64, usize> = BTreeMap::new();
    let mut request = ScanRequest::new(KeyRange::all());
    loop {
        let batch = txn.scan(request.clone()).await.unwrap();
        for (key, value) in &batch.rows {
            let id = id_of(key);
            assert_eq!(value[..], id.to_be_bytes(), "wrong value under {key:?}");
            *present.entry(id).or_default() += 1;
        }
        match request.resume_after(&batch) {
            Some(next) => request = next,
            None => break,
        }
    }
    (storage.visible_ts().into_raw(), present)
}

#[tokio::test]
/// Assert crashes at random points lose no acknowledged commit and tear no transaction.
async fn crash_recovery_loses_no_acknowledged_commits() {
    let cycles: u64 = std::env::var("CRASH_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let dir = tempfile::tempdir().unwrap();
    let mut acked: BTreeMap<u64, u64> = BTreeMap::new();
    let mut deletes: BTreeMap<u64, u64> = BTreeMap::new();
    let mut recovered_ts = 0;
    let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
    let mut total_acked = 0;

    for cycle in 0..cycles {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let kill_after = Duration::from_millis(30 + rng % 400);
        let (start_ts, report) = run_child(dir.path(), cycle * 1_000_000_000, kill_after);
        assert_eq!(
            start_ts, recovered_ts,
            "cycle {cycle}: child resumed at the wrong timestamp"
        );
        for &ts in report.acked.values() {
            assert!(
                ts > recovered_ts,
                "cycle {cycle}: commit timestamp {ts} reused"
            );
        }
        total_acked += report.acked.len();
        acked.extend(&report.acked);
        deletes.extend(&report.deletes);

        let (visible, present) = recover(dir.path()).await;
        let victims: BTreeSet<u64> = deletes.values().copied().collect();
        // All-or-nothing per transaction.
        for (&id, &count) in &present {
            assert_eq!(
                count as u64, KEYS_PER_TXN,
                "cycle {cycle}: transaction {id} is torn"
            );
        }
        // Acknowledged commits survive unless a delete targeted them.
        for (&id, &ts) in &acked {
            assert!(
                ts <= visible,
                "cycle {cycle}: acknowledged ts {ts} above recovered {visible}"
            );
            assert!(
                present.contains_key(&id) || victims.contains(&id),
                "cycle {cycle}: acknowledged transaction {id} (ts {ts}) lost"
            );
        }
        // A delete transaction's own writes and its deletion commit together.
        for (&id, &victim) in &deletes {
            if present.contains_key(&id) {
                assert!(
                    !present.contains_key(&victim),
                    "cycle {cycle}: delete {id} committed but victim {victim} survived"
                );
            }
        }
        assert!(visible >= recovered_ts);
        recovered_ts = visible;
    }
    assert!(
        total_acked > 100,
        "too few commits ({total_acked}) to be meaningful"
    );
}
