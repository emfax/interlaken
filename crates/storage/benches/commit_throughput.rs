//! Commit throughput of the single sequencer thread (ADR-0005).
//!
//! Concurrent clients commit transactions for a fixed time. Each run reports
//! client-side throughput and latency (p50, p99 and max, the last for
//! spotting fsync stalls), plus the sequencer's own counters:
//! mean group size, and how busy the sequencer thread was. A busy fraction
//! near 100% means the single sequencer, not the clients or the disk wait,
//! limits throughput; `cap` extrapolates the rate the sequencer could reach
//! if it were busy all the time at the measured per-transaction cost.
//!
//! Run with `cargo bench -p interlaken-storage --bench commit_throughput`.
//! Optional arguments filter runs by substring (for example `sync` or
//! `rw/64`). Environment variables:
//!
//! - `BENCH_SECS`: seconds per run (default 2).
//! - `BENCH_DIR`: directory for the temporary databases (default: the
//!   system temp dir). Put it on the disk you want to measure; a tmpfs
//!   makes `sync` meaningless.

use std::ops::Bound;
use std::time::{Duration, Instant};

use bytes::Bytes;
use interlaken_storage::encoding::{Datum, SortOrder, encode_tuple};
use interlaken_storage::{
    CommitStats, Durability, Error, FjallStorage, Isolation, KeyRange, ScanRequest, Storage,
    StorageOptions, TxnOptions,
};

/// Rows preloaded for the read-write workload to read.
const PRELOADED: i64 = 10_000;

/// Size of every written value.
const VALUE_LEN: usize = 100;

/// Concurrent client counts to measure.
const CLIENTS: [usize; 5] = [1, 4, 16, 64, 256];

/// Table id of the preloaded rows.
const READ_TABLE: i32 = 1;

/// Table id the clients write to.
const WRITE_TABLE: i32 = 2;

/// The transaction each client runs in a loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    /// REPEATABLE READ, one write to a fresh key, no reads: the bare pipeline.
    BlindWrite,
    /// SERIALIZABLE, four point reads and a 16-row scan of preloaded rows,
    /// then one write to a fresh key: validation with a read set.
    ReadWrite,
}

impl Workload {
    /// Return the short name used in output and filters.
    fn name(self) -> &'static str {
        match self {
            Self::BlindWrite => "blind",
            Self::ReadWrite => "rw",
        }
    }
}

/// Encode a `(table, id)` key.
fn key(table: i32, id: i64) -> Bytes {
    Bytes::from(
        encode_tuple(&[
            (Datum::Int4(table), SortOrder::ASC),
            (Datum::Int8(id), SortOrder::ASC),
        ])
        .unwrap(),
    )
}

/// Return a pseudo-random number (xorshift64*), advancing `state`.
fn next_random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Results of one client.
struct ClientResult {
    committed: u64,
    aborted: u64,
    latencies: Vec<Duration>,
}

/// Commit transactions until `deadline`; return counts and per-commit latencies.
async fn client(
    storage: FjallStorage,
    workload: Workload,
    id: usize,
    deadline: Instant,
) -> ClientResult {
    let mut rng = 0x9E37_79B9_7F4A_7C15 ^ (id as u64 + 1).wrapping_mul(0xD1B5_4A32_D192_ED03);
    let value = Bytes::from(vec![0xAB; VALUE_LEN]);
    let mut result = ClientResult {
        committed: 0,
        aborted: 0,
        latencies: Vec::new(),
    };
    let mut seq: i64 = 0;
    while Instant::now() < deadline {
        let started = Instant::now();
        let isolation = match workload {
            Workload::BlindWrite => Isolation::RepeatableRead,
            Workload::ReadWrite => Isolation::Serializable,
        };
        let txn = storage.begin(TxnOptions::new(isolation)).await.unwrap();
        if workload == Workload::ReadWrite {
            let keys = (0..4)
                .map(|_| {
                    key(
                        READ_TABLE,
                        (next_random(&mut rng) % PRELOADED as u64) as i64,
                    )
                })
                .collect();
            txn.get_many(keys).await.unwrap();
            let start = (next_random(&mut rng) % PRELOADED as u64) as i64;
            let mut scan = ScanRequest::new(KeyRange::new(
                Bound::Included(key(READ_TABLE, start)),
                Bound::Unbounded,
            ));
            scan.limit_rows = 16;
            txn.scan(scan).await.unwrap();
        }
        seq += 1;
        let fresh = ((id as i64) << 40) | seq;
        txn.put(key(WRITE_TABLE, fresh), value.clone()).unwrap();
        match txn.commit().await {
            Ok(_) => {
                result.committed += 1;
                result.latencies.push(started.elapsed());
            }
            Err(Error::SerializationFailure(_)) => result.aborted += 1,
            Err(other) => panic!("commit failed: {other}"),
        }
    }
    result
}

/// Preload the rows the read-write workload reads.
async fn preload(storage: &FjallStorage) {
    let value = Bytes::from(vec![0xCD; VALUE_LEN]);
    for chunk in 0..PRELOADED / 1000 {
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        for id in chunk * 1000..(chunk + 1) * 1000 {
            txn.put(key(READ_TABLE, id), value.clone()).unwrap();
        }
        txn.commit().await.unwrap();
    }
}

/// Return the `q` quantile of sorted latencies.
fn quantile(sorted: &[Duration], q: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[index]
}

/// Run one configuration and print a result row.
async fn run(workload: Workload, durability: Durability, clients: usize, secs: f64) {
    let base = std::env::var_os("BENCH_DIR").map_or_else(std::env::temp_dir, Into::into);
    let dir = tempfile::tempdir_in(base).unwrap();
    let options = StorageOptions {
        durability,
        ..StorageOptions::default()
    };
    let storage = FjallStorage::open(dir.path(), options).unwrap();
    if workload == Workload::ReadWrite {
        preload(&storage).await;
    }

    let before: CommitStats = storage.commit_stats();
    let started = Instant::now();
    let deadline = started + Duration::from_secs_f64(secs);
    let tasks: Vec<_> = (0..clients)
        .map(|id| tokio::spawn(client(storage.clone(), workload, id, deadline)))
        .collect();
    let mut committed = 0;
    let mut aborted = 0;
    let mut latencies = Vec::new();
    for task in tasks {
        let mut result = task.await.unwrap();
        committed += result.committed;
        aborted += result.aborted;
        latencies.append(&mut result.latencies);
    }
    let wall = started.elapsed();
    let stats = storage.commit_stats().since(&before);
    latencies.sort_unstable();

    let busy = stats.validation + stats.write;
    let busy_fraction = busy.as_secs_f64() / wall.as_secs_f64();
    let cap = if busy.is_zero() {
        0.0
    } else {
        stats.committed as f64 / busy.as_secs_f64()
    };
    let per_txn_validation_us = if stats.committed == 0 {
        0.0
    } else {
        stats.validation.as_secs_f64() * 1e6 / stats.committed as f64
    };
    let per_group_write_us = if stats.groups == 0 {
        0.0
    } else {
        stats.write.as_secs_f64() * 1e6 / stats.groups as f64
    };
    println!(
        "{:<6} {:<8} {:>7} {:>10.0} {:>7} {:>7.1} {:>9.1} {:>9.1} {:>10.1} {:>6.1}% {:>9.1} {:>10.1} {:>10.0}",
        workload.name(),
        format!("{durability:?}").to_lowercase(),
        clients,
        committed as f64 / wall.as_secs_f64(),
        aborted,
        stats.mean_group_size(),
        quantile(&latencies, 0.5).as_secs_f64() * 1e6,
        quantile(&latencies, 0.99).as_secs_f64() * 1e6,
        latencies.last().copied().unwrap_or_default().as_secs_f64() * 1e6,
        busy_fraction * 100.0,
        per_txn_validation_us,
        per_group_write_us,
        cap,
    );
}

/// Run every configuration that matches the command-line filters.
fn main() {
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let secs: f64 = std::env::var("BENCH_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.0);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    println!(
        "{:<6} {:<8} {:>7} {:>10} {:>7} {:>7} {:>9} {:>9} {:>10} {:>7} {:>9} {:>10} {:>10}",
        "load",
        "durab.",
        "clients",
        "commits/s",
        "aborts",
        "group",
        "p50 µs",
        "p99 µs",
        "max µs",
        "seq",
        "val µs/tx",
        "write µs/g",
        "cap/s"
    );
    for workload in [Workload::BlindWrite, Workload::ReadWrite] {
        for durability in [Durability::Sync, Durability::Buffered] {
            for clients in CLIENTS {
                let label = format!(
                    "{}/{}/{clients}",
                    workload.name(),
                    format!("{durability:?}").to_lowercase()
                );
                if !filters.is_empty() && !filters.iter().any(|f| label.contains(f.as_str())) {
                    continue;
                }
                runtime.block_on(run(workload, durability, clients, secs));
            }
        }
    }
}
