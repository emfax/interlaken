//! Crash and power-loss harness: crash mid-commit, recover, verify (ADR-0009 phases 6–7, ADR-0005).
//!
//! The parent test re-runs this test binary as a child that commits as fast
//! as it can while version GC runs every millisecond. The child prints a
//! line for each acknowledged commit, and for each delete it is about to
//! attempt. The parent kills it (`SIGKILL`) at a random moment, optionally
//! injects power loss, reopens the database and checks:
//!
//! - every transaction is all-or-nothing: its four keys, and a delete
//!   transaction's removal of its victim, appear together or not at all;
//! - the recovered commit clock separates survivors from losses: every
//!   surviving acknowledged commit is at or below it, every lost one above
//!   it, so what survives is a prefix of the commit order;
//! - when commits must survive, none is lost unless a later committed
//!   delete removed it;
//! - the next run's commits are numbered after the recovered clock.
//!
//! It then runs the next cycle on the same directory, so recovery itself is
//! crashed into repeatedly. `CRASH_CYCLES` sets the cycle count (default 6).
//!
//! `SIGKILL` loses no data the process already handed to the kernel, so the
//! process-crash tests check crash consistency and recovery: commits
//! acknowledged under [`Durability::Buffered`] survive too. The power-loss
//! tests run on a [`LazyFs`] mount and clear its cache of unsynced writes
//! after each kill, as a power cut would. There, every commit acknowledged
//! under [`Durability::Sync`] must survive, while `Buffered` commits may be
//! lost but the database must reopen consistent. They need `LAZYFS_ROOT`
//! (see `scripts/power-loss.sh`) and skip with a message without it.

#[allow(dead_code, unused_imports)]
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use interlaken_storage::{
    Durability, FjallStorage, Key, KeyRange, ScanRequest, Storage, StorageOptions, TxnOptions,
};
use support::{LAZYFS_ROOT, LazyFs};

/// Keys each transaction writes.
const KEYS_PER_TXN: u64 = 4;

/// Memtable size in power-loss runs: the smallest the engine allows, 1 MiB.
const POWER_LOSS_MEMTABLE_BYTES: u64 = 1024 * 1024;

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

/// Encode a durability mode for the child's environment.
fn durability_name(durability: Durability) -> &'static str {
    match durability {
        Durability::Sync => "sync",
        Durability::Buffered => "buffered",
    }
}

/// What the child reported.
#[derive(Default)]
struct Report {
    /// Acknowledged commits: ID and commit timestamp.
    acked: BTreeMap<u64, u64>,
    /// Delete transactions attempted: delete ID to victim ID.
    deletes: BTreeMap<u64, u64>,
}

/// How each cycle ends.
#[derive(Clone, Copy)]
enum Fault<'a> {
    /// `SIGKILL` the child.
    ProcessKill,
    /// `SIGKILL` the child, then drop every unsynced write on the LazyFS mount.
    PowerLoss(&'a LazyFs),
}

#[test]
#[ignore = "run by the crash and power-loss tests as a child process"]
/// Commit transactions forever, reporting acknowledgements on stdout.
fn crash_child() {
    let dir = std::env::var("CRASH_DIR").expect("CRASH_DIR");
    let base: u64 = std::env::var("CRASH_BASE").unwrap().parse().unwrap();
    let durability = match std::env::var("CRASH_DURABILITY").as_deref() {
        Ok("buffered") => Durability::Buffered,
        _ => Durability::Sync,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let options = StorageOptions {
            durability,
            gc_interval: Some(Duration::from_millis(1)),
            gc_chunk_versions: 16,
            memtable_bytes: std::env::var("CRASH_MEMTABLE_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(StorageOptions::default().memtable_bytes),
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

/// Run the child for a random time, kill it, and return its start timestamp and report.
fn run_child(
    dir: &Path,
    base: u64,
    durability: Durability,
    fault: Fault<'_>,
    kill_after: Duration,
) -> (u64, Report) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    if matches!(fault, Fault::PowerLoss(_)) {
        // Small memtables make fjall flush tables and rotate journals often,
        // so power loss also strikes those paths, and `Buffered` runs keep
        // whatever a flush synced.
        command.env(
            "CRASH_MEMTABLE_BYTES",
            POWER_LOSS_MEMTABLE_BYTES.to_string(),
        );
    }
    let mut child = command
        .args([
            "--exact",
            "crash_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("CRASH_DIR", dir)
        .env("CRASH_BASE", base.to_string())
        .env("CRASH_DURABILITY", durability_name(durability))
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
            .recv_timeout(Duration::from_secs(60))
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
async fn recover(dir: &Path) -> (u64, BTreeMap<u64, usize>) {
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

/// Every acknowledgement and attempted delete reported so far, minus commits known lost.
#[derive(Default)]
struct Ledger {
    /// Acknowledged commits not yet found lost: ID and commit timestamp.
    acked: BTreeMap<u64, u64>,
    /// Delete transactions attempted: delete ID to victim ID.
    deletes: BTreeMap<u64, u64>,
}

impl Ledger {
    /// Assert the recovered state is consistent with the ledger, prune lost
    /// commits from it, and return how many acknowledged commits were lost.
    ///
    /// # Panics
    ///
    /// Panics on a torn transaction, a surviving victim of a committed
    /// delete, a recovered clock that does not separate surviving from lost
    /// commits, or, when `must_survive`, any lost commit.
    fn check(
        &mut self,
        cycle: u64,
        visible: u64,
        present: &BTreeMap<u64, usize>,
        must_survive: bool,
    ) -> usize {
        // All-or-nothing per transaction.
        for (&id, &count) in present {
            assert_eq!(
                count as u64, KEYS_PER_TXN,
                "cycle {cycle}: transaction {id} is torn"
            );
        }
        // A transaction committed if its keys are present, or if a committed
        // delete removed them.
        let mut committed: BTreeSet<u64> = present.keys().copied().collect();
        loop {
            let before = committed.len();
            for (&id, &victim) in &self.deletes {
                if committed.contains(&id) {
                    committed.insert(victim);
                }
            }
            if committed.len() == before {
                break;
            }
        }
        // A committed delete transaction removed its victim.
        for (&id, &victim) in &self.deletes {
            if committed.contains(&id) {
                assert!(
                    !present.contains_key(&victim),
                    "cycle {cycle}: delete {id} committed but victim {victim} survived"
                );
            }
        }
        // The recovered clock covers exactly the surviving commits.
        let lost: Vec<(u64, u64)> = self
            .acked
            .iter()
            .filter(|(id, _)| !committed.contains(id))
            .map(|(&id, &ts)| (id, ts))
            .collect();
        for (&id, &ts) in &self.acked {
            if committed.contains(&id) {
                assert!(
                    ts <= visible,
                    "cycle {cycle}: transaction {id} survived at ts {ts} above recovered {visible}"
                );
            }
        }
        for &(id, ts) in &lost {
            assert!(
                !must_survive,
                "cycle {cycle}: acknowledged transaction {id} (ts {ts}) lost"
            );
            assert!(
                ts > visible,
                "cycle {cycle}: transaction {id} lost at ts {ts}, at or below recovered {visible}: \
                 survivors are not a prefix of the commit order"
            );
        }
        for &(id, _) in &lost {
            self.acked.remove(&id);
        }
        lost.len()
    }
}

/// Totals over a run of crash cycles.
#[derive(Debug, Default)]
struct Totals {
    /// Commits acknowledged.
    acked: usize,
    /// Acknowledged commits lost to injected power loss.
    lost: usize,
}

/// Run `cycles` crash cycles on the database in `dir` and return the totals.
///
/// # Panics
///
/// Panics if any cycle's recovered state fails [`Ledger::check`], the child
/// resumes at the wrong timestamp or reuses one, or too few commits were
/// acknowledged to be meaningful.
async fn run_cycles(dir: &Path, durability: Durability, fault: Fault<'_>, cycles: u64) -> Totals {
    let must_survive = durability == Durability::Sync || matches!(fault, Fault::ProcessKill);
    let mut ledger = Ledger::default();
    let mut totals = Totals::default();
    let mut recovered_ts = 0;
    let mut rng: u64 = 0x2545_F491_4F6C_DD1D;

    for cycle in 0..cycles {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let kill_after = Duration::from_millis(30 + rng % 400);
        let (start_ts, report) =
            run_child(dir, cycle * 1_000_000_000, durability, fault, kill_after);
        if let Fault::PowerLoss(lazyfs) = fault {
            lazyfs.clear_cache();
        }
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
        totals.acked += report.acked.len();
        ledger.acked.extend(&report.acked);
        ledger.deletes.extend(&report.deletes);

        let (visible, present) = recover(dir).await;
        totals.lost += ledger.check(cycle, visible, &present, must_survive);
        if must_survive {
            assert!(
                visible >= recovered_ts,
                "cycle {cycle}: clock went backwards"
            );
        }
        recovered_ts = visible;
    }
    assert!(
        totals.acked > 100,
        "too few commits ({}) to be meaningful",
        totals.acked
    );
    totals
}

/// Return the cycle count from `CRASH_CYCLES`, defaulting to 6.
fn cycles() -> u64 {
    std::env::var("CRASH_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6)
}

/// Run the power-loss cycles on a fresh LazyFS mount, or skip if `LAZYFS_ROOT` is unset.
async fn run_power_loss(durability: Durability) -> Option<Totals> {
    let Some(lazyfs) = LazyFs::from_env() else {
        eprintln!(
            "skipping power-loss test: set {LAZYFS_ROOT} to a built LazyFS checkout \
             (scripts/build-lazyfs.sh, then scripts/power-loss.sh)"
        );
        return None;
    };
    let dir = lazyfs.path().join("db");
    let totals = run_cycles(&dir, durability, Fault::PowerLoss(&lazyfs), cycles()).await;
    eprintln!(
        "power loss under {}: {} cycles, {} commits acknowledged, {} lost",
        durability_name(durability),
        cycles(),
        totals.acked,
        totals.lost
    );
    Some(totals)
}

#[tokio::test]
/// Assert process crashes at random points lose no `Sync` commit and tear no transaction.
async fn crash_recovery_loses_no_acknowledged_commits() {
    let dir = tempfile::tempdir().unwrap();
    let totals = run_cycles(dir.path(), Durability::Sync, Fault::ProcessKill, cycles()).await;
    assert_eq!(totals.lost, 0);
}

#[tokio::test]
/// Assert process crashes lose no `Buffered` commit either: the kernel still has them.
async fn crash_recovery_loses_no_buffered_commits() {
    let dir = tempfile::tempdir().unwrap();
    let totals = run_cycles(
        dir.path(),
        Durability::Buffered,
        Fault::ProcessKill,
        cycles(),
    )
    .await;
    assert_eq!(totals.lost, 0);
}

#[tokio::test]
/// Assert injected power loss loses no commit acknowledged under `Sync` (needs `LAZYFS_ROOT`).
async fn power_loss_loses_no_sync_commits() {
    if let Some(totals) = run_power_loss(Durability::Sync).await {
        assert_eq!(totals.lost, 0);
    }
}

#[tokio::test]
/// Assert injected power loss leaves a `Buffered` database consistent (needs `LAZYFS_ROOT`).
///
/// Acknowledged commits may be lost, but only a suffix of the commit order,
/// never part of a transaction, and the database always reopens.
async fn power_loss_keeps_buffered_commits_consistent() {
    run_power_loss(Durability::Buffered).await;
}
