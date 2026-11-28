//! Isolation checking with Jepsen's Elle (ADR-0009 phase 6, milestone M6).
//!
//! Concurrent tasks run Elle's list-append workload: each transaction is a
//! few micro-operations that append a unique integer to a list or read a
//! whole list. Lists are stored as values, so an append is a read-modify-
//! write. Every micro-operation is its own statement, so later reads see the
//! transaction's earlier appends. The history (invocations and completions,
//! in real-time order) is written as EDN under `target/elle/` and checked by
//! `elle-cli`, which infers dependencies from the values alone, independent
//! of the engine's timestamps.
//!
//! Each isolation level is checked against its model:
//!
//! | Engine level | Must satisfy | Must violate |
//! | --- | --- | --- |
//! | SERIALIZABLE | `strict-serializable` | |
//! | REPEATABLE READ | `snapshot-isolation` | `serializable` (write skew) |
//! | READ COMMITTED | `read-committed` | |
//!
//! Elle needs Java and the `elle-cli` standalone jar, found through the
//! `ELLE_CLI` environment variable or at `target/elle-cli.jar`. Without
//! them the histories are still generated and saved, and the check is
//! skipped with a note. `ELLE_RUNS` sets how many histories each test
//! checks (default 3).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use interlaken_storage::{
    Durability, Error, FjallStorage, Isolation, Key, Storage, StorageOptions, TxnOptions,
};

/// Concurrent client processes.
const PROCESSES: u64 = 8;

/// Transactions per process.
const TXNS: u64 = 60;

/// Distinct lists.
const KEYS: u64 = 8;

/// One micro-operation, with the read result once known.
#[derive(Clone, Debug)]
enum Mop {
    Append(u64, i64),
    Read(u64, Option<Vec<i64>>),
}

/// Render micro-operations as an EDN vector.
fn edn(mops: &[Mop]) -> String {
    let mut out = String::from("[");
    for (i, mop) in mops.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match mop {
            Mop::Append(k, v) => write!(out, "[:append {k} {v}]").unwrap(),
            Mop::Read(k, None) => write!(out, "[:r {k} nil]").unwrap(),
            Mop::Read(k, Some(list)) => {
                let items: Vec<String> = list.iter().map(i64::to_string).collect();
                write!(out, "[:r {k} [{}]]", items.join(" ")).unwrap();
            }
        }
    }
    out.push(']');
    out
}

/// Records operations in real-time order with consecutive indices.
struct Recorder {
    start: Instant,
    lines: Mutex<Vec<String>>,
}

impl Recorder {
    /// Append one history operation.
    fn record(&self, kind: &str, process: u64, mops: &[Mop]) {
        let mut lines = self.lines.lock().unwrap();
        let index = lines.len();
        let time = self.start.elapsed().as_nanos();
        lines.push(format!(
            "{{:type :{kind}, :f :txn, :value {}, :process {process}, :time {time}, :index {index}}}",
            edn(mops)
        ));
    }
}

/// Encode a list as concatenated big-endian integers.
fn encode(list: &[i64]) -> Bytes {
    Bytes::from(
        list.iter()
            .flat_map(|v| v.to_be_bytes())
            .collect::<Vec<u8>>(),
    )
}

/// Decode a stored list.
fn decode(bytes: &[u8]) -> Vec<i64> {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|&c| i64::from_be_bytes(c))
        .collect()
}

/// Return the storage key of list `k`.
fn key(k: u64) -> Key {
    Bytes::from(format!("list{k}"))
}

/// Run one process's transactions.
async fn process(
    storage: FjallStorage,
    recorder: Arc<Recorder>,
    next_value: Arc<AtomicI64>,
    process: u64,
    isolation: Isolation,
) {
    let mut rng = (process + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut next = || {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    for _ in 0..TXNS {
        let mops: Vec<Mop> = (0..1 + next() % 4)
            .map(|_| {
                let k = next() % KEYS;
                if next() % 2 == 0 {
                    Mop::Append(k, next_value.fetch_add(1, Ordering::Relaxed))
                } else {
                    Mop::Read(k, None)
                }
            })
            .collect();
        recorder.record("invoke", process, &mops);
        let txn = storage.begin(TxnOptions::new(isolation)).await.unwrap();
        let mut done = Vec::with_capacity(mops.len());
        for mop in &mops {
            let current = txn.get_many(vec![key(mop_key(mop))]).await.unwrap()[0]
                .as_deref()
                .map(decode)
                .unwrap_or_default();
            match mop {
                Mop::Append(k, v) => {
                    let mut list = current;
                    list.push(*v);
                    txn.put(key(*k), encode(&list)).unwrap();
                    done.push(mop.clone());
                }
                Mop::Read(k, _) => done.push(Mop::Read(*k, Some(current))),
            }
            txn.next_statement();
        }
        match txn.commit().await {
            Ok(_) => recorder.record("ok", process, &done),
            Err(Error::SerializationFailure(_)) => recorder.record("fail", process, &mops),
            Err(other) => panic!("unexpected commit error: {other}"),
        }
    }
}

/// Return the list a micro-operation touches.
fn mop_key(mop: &Mop) -> u64 {
    match mop {
        Mop::Append(k, _) | Mop::Read(k, _) => *k,
    }
}

/// Run the workload at `isolation` and write the history to `path`.
async fn generate(isolation: Isolation, path: &Path) {
    let dir = tempfile::tempdir().unwrap();
    let options = StorageOptions {
        durability: Durability::Buffered,
        gc_interval: Some(std::time::Duration::from_millis(2)),
        ..StorageOptions::default()
    };
    let storage = FjallStorage::open(dir.path(), options).unwrap();
    let recorder = Arc::new(Recorder {
        start: Instant::now(),
        lines: Mutex::new(Vec::new()),
    });
    let next_value = Arc::new(AtomicI64::new(1));
    let tasks: Vec<_> = (0..PROCESSES)
        .map(|p| {
            tokio::spawn(process(
                storage.clone(),
                recorder.clone(),
                next_value.clone(),
                p,
                isolation,
            ))
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let lines = recorder.lines.lock().unwrap();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// Return the workspace `target` directory.
fn target_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

/// Return the elle-cli jar, if available.
fn elle_jar() -> Option<PathBuf> {
    let jar = std::env::var_os("ELLE_CLI")
        .map_or_else(|| target_dir().join("elle-cli.jar"), PathBuf::from);
    jar.exists().then_some(jar)
}

/// Generate `ELLE_RUNS` histories at `isolation` named after `name`; return their paths.
async fn histories(name: &str, isolation: Isolation) -> Vec<PathBuf> {
    let runs: usize = std::env::var("ELLE_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let out = target_dir().join("elle");
    std::fs::create_dir_all(&out).unwrap();
    let mut paths = Vec::new();
    for run in 0..runs {
        let path = out.join(format!("{name}-{run}.edn"));
        generate(isolation, &path).await;
        paths.push(path);
    }
    paths
}

/// Check every history against `model`; return each one's verdict and, for
/// invalid ones when `explain` is set, Elle's explanation.
///
/// All histories go to one Elle run; only invalid ones are re-run for
/// details. Returns `None` when Elle isn't available.
fn check(paths: &[PathBuf], model: &str, explain: bool) -> Option<Vec<(bool, String)>> {
    let Some(jar) = elle_jar() else {
        eprintln!(
            "elle-cli not found (set ELLE_CLI or put it at target/elle-cli.jar); \
             histories saved, Elle check skipped"
        );
        return None;
    };
    let run = |files: &[PathBuf], format: &str| {
        let output = Command::new("java")
            .arg("-jar")
            .arg(&jar)
            .args(["-m", "list-append", "-c", model, "-v", format])
            .args(files)
            .output()
            .expect("failed to run java");
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let summary = run(paths, "text");
    let verdicts = paths
        .iter()
        .map(|path| {
            let name = path.display().to_string();
            let line = summary
                .lines()
                .find(|l| l.starts_with(&name))
                .unwrap_or_else(|| panic!("Elle gave no verdict for {name}:\n{summary}"));
            match line.rsplit(char::is_whitespace).next() {
                Some("true") => (true, String::new()),
                Some("false") if explain => (false, run(std::slice::from_ref(path), "json")),
                Some("false") => (false, String::new()),
                _ => panic!("Elle could not decide {name}: {line}"),
            }
        })
        .collect();
    Some(verdicts)
}

/// Assert every history satisfies `model`.
fn assert_valid(paths: &[PathBuf], model: &str) {
    if let Some(verdicts) = check(paths, model, true) {
        for (path, (valid, output)) in paths.iter().zip(verdicts) {
            assert!(
                valid,
                "{} violates {model}:\n{}",
                path.display(),
                &output[..output.len().min(4000)]
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert SERIALIZABLE histories are strict serializable.
async fn serializable_is_strict_serializable() {
    let paths = histories("serializable", Isolation::Serializable).await;
    assert_valid(&paths, "strict-serializable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert REPEATABLE READ histories satisfy snapshot isolation but Elle finds them not serializable.
async fn repeatable_read_is_snapshot_isolation_but_not_serializable() {
    let paths = histories("repeatable-read", Isolation::RepeatableRead).await;
    assert_valid(&paths, "snapshot-isolation");
    // One invalid history is enough; skip fetching explanations.
    if let Some(verdicts) = check(&paths, "serializable", false) {
        assert!(
            verdicts.iter().any(|(valid, _)| !valid),
            "Elle found no anomaly in any REPEATABLE READ history; the harness may be blind"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert READ COMMITTED histories satisfy read committed.
async fn read_committed_is_read_committed() {
    let paths = histories("read-committed", Isolation::ReadCommitted).await;
    assert_valid(&paths, "read-committed");
}
