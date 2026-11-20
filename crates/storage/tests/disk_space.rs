//! Disk-space soak: on-disk bytes level off under sustained updates (ADR-0009 phase 7).
//!
//! Workers overwrite a fixed set of keys at a target rate while version
//! GC runs in the background, so the live data never grows: every new
//! version makes an older one garbage. GC removes garbage with physical
//! fjall deletes, and the space only comes back once journals are evicted
//! and compaction drops the tombstones and the versions they shadow. A
//! sampler records [`StorageMetrics::disk_space_bytes`] at a fixed interval.
//! After a warm-up third, the later half of the run must not peak much
//! higher than the earlier half: the footprint has levelled off.
//!
//! - `disk_space_levels_off_short` runs for 12 seconds at 32 MiB/s with the smallest
//!   journal cap (64 MiB) and 4 MiB memtables, as part of the normal suite.
//! - `disk_space_levels_off_under_sustained_updates` runs with fjall's
//!   default limits for `SOAK_SECS` seconds (default 600) and is
//!   `#[ignore]`d; run it with `scripts/disk-space-soak.sh`.
//!
//! [`StorageMetrics::disk_space_bytes`]: interlaken_storage::StorageMetrics::disk_space_bytes

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use interlaken_storage::{Durability, FjallStorage, Key, Storage, StorageOptions, TxnOptions};

/// Bytes in a mebibyte.
const MIB: u64 = 1024 * 1024;

/// What one soak run does.
#[derive(Clone, Copy, Debug)]
struct Soak {
    /// How long the workers write.
    duration: Duration,
    /// Distinct keys the workers overwrite.
    keys: u64,
    /// Bytes in each value.
    value_bytes: usize,
    /// Keys each transaction overwrites.
    keys_per_txn: u64,
    /// Concurrent writers.
    workers: u64,
    /// Target commit rate, in key and value bytes per second.
    bytes_per_sec: u64,
    /// How often disk space is sampled.
    sample_every: Duration,
    /// `StorageOptions::max_journal_bytes`.
    max_journal_bytes: u64,
    /// `StorageOptions::memtable_bytes`.
    memtable_bytes: u64,
}

/// One disk-space sample.
#[derive(Clone, Copy, Debug)]
struct Sample {
    /// Time since the workers started.
    elapsed: Duration,
    /// `StorageMetrics::disk_space_bytes` at that time.
    disk_bytes: u64,
    /// Key and value bytes committed so far.
    written_bytes: u64,
    /// Versions GC has removed so far.
    removed_versions: u64,
}

/// Encode key `i`.
fn key(i: u64) -> Key {
    Bytes::from(format!("soak/{i:08}"))
}

/// Construct an incompressible value of `len` bytes from a seed (xorshift).
fn value(seed: u64, len: usize) -> Bytes {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    Bytes::from(out)
}

/// Parse an environment variable as a number, or return `default`.
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Convert bytes to mebibytes for display.
fn mib(bytes: u64) -> f64 {
    bytes as f64 / MIB as f64
}

/// Run the soak and return the samples taken.
async fn run_soak(soak: Soak) -> Vec<Sample> {
    let dir = tempfile::tempdir().unwrap();
    let storage = FjallStorage::open(
        dir.path(),
        StorageOptions {
            durability: Durability::Buffered,
            gc_interval: Some(Duration::from_millis(100)),
            max_journal_bytes: soak.max_journal_bytes,
            memtable_bytes: soak.memtable_bytes,
            ..StorageOptions::default()
        },
    )
    .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let written = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let workers: Vec<_> = (0..soak.workers)
        .map(|worker| {
            let storage = storage.clone();
            let stop = stop.clone();
            let written = written.clone();
            tokio::spawn(async move {
                let mut rng = 0x9E37_79B9_7F4A_7C15 ^ (worker + 1);
                while !stop.load(Ordering::Relaxed) {
                    let txn = storage.begin(TxnOptions::default()).await.unwrap();
                    let mut bytes = 0;
                    for _ in 0..soak.keys_per_txn {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        let k = key(rng % soak.keys);
                        let v = value(rng, soak.value_bytes);
                        bytes += (k.len() + v.len()) as u64;
                        txn.put(k, v).unwrap();
                    }
                    // Concurrent overwrites of one key conflict; skip those.
                    if txn.commit().await.is_ok() {
                        let total = written.fetch_add(bytes, Ordering::Relaxed) + bytes;
                        // Stay at the target rate rather than saturating the disk.
                        let due = Duration::from_secs_f64(total as f64 / soak.bytes_per_sec as f64);
                        if let Some(ahead) = due.checked_sub(start.elapsed()) {
                            tokio::time::sleep(ahead).await;
                        }
                    }
                }
            })
        })
        .collect();

    let mut samples = Vec::new();
    while start.elapsed() < soak.duration {
        tokio::time::sleep(soak.sample_every).await;
        let metrics = storage.metrics().await.unwrap();
        let sample = Sample {
            elapsed: start.elapsed(),
            disk_bytes: metrics.disk_space_bytes,
            written_bytes: written.load(Ordering::Relaxed),
            removed_versions: metrics.gc.versions_removed,
        };
        eprintln!(
            "soak t={:>6.1}s disk={:>8.1} MiB written={:>10.1} MiB gc_removed={}",
            sample.elapsed.as_secs_f64(),
            mib(sample.disk_bytes),
            mib(sample.written_bytes),
            sample.removed_versions,
        );
        samples.push(sample);
    }
    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        worker.await.unwrap();
    }
    samples
}

/// Assert disk space levelled off: after the warm-up third, the later half
/// of the samples peaks at most 25% (plus 8 MiB of slack) above the earlier half.
fn assert_levels_off(samples: &[Sample]) {
    assert!(samples.len() >= 6, "too few samples ({})", samples.len());
    let steady = &samples[samples.len() / 3..];
    let (earlier, later) = steady.split_at(steady.len() / 2);
    let peak = |s: &[Sample]| s.iter().map(|s| s.disk_bytes).max().unwrap();
    let (earlier_peak, later_peak) = (peak(earlier), peak(later));
    let last = samples.last().unwrap();
    eprintln!(
        "soak summary: {:.0} s, {:.1} MiB written, {} versions removed, \
         peak {:.1} MiB earlier vs {:.1} MiB later, final {:.1} MiB",
        last.elapsed.as_secs_f64(),
        mib(last.written_bytes),
        last.removed_versions,
        mib(earlier_peak),
        mib(later_peak),
        mib(last.disk_bytes),
    );
    assert!(last.removed_versions > 0, "GC removed nothing");
    assert!(
        later_peak <= earlier_peak + earlier_peak / 4 + 8 * MIB,
        "disk space still growing: peak {:.1} MiB in the earlier half of the steady state, \
         {:.1} MiB in the later half",
        mib(earlier_peak),
        mib(later_peak),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert disk space levels off within seconds under the smallest journal cap.
async fn disk_space_levels_off_short() {
    let samples = run_soak(Soak {
        duration: Duration::from_secs(12),
        keys: 5_000,
        value_bytes: 256,
        keys_per_txn: 16,
        workers: 4,
        sample_every: Duration::from_millis(500),
        bytes_per_sec: 32 * MIB,
        max_journal_bytes: 64 * MIB,
        memtable_bytes: 4 * MIB,
    })
    .await;
    assert_levels_off(&samples);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "long soak; run with scripts/disk-space-soak.sh (SOAK_SECS sets the duration)"]
/// Assert disk space levels off over a long sustained-update run.
///
/// `SOAK_SECS` (default 600), `SOAK_MIB_PER_SEC` (default 16), `SOAK_JOURNAL_MIB`
/// (default 512, fjall's default) and `SOAK_MEMTABLE_MIB` (default 64) configure the run.
async fn disk_space_levels_off_under_sustained_updates() {
    let samples = run_soak(Soak {
        duration: Duration::from_secs(env_u64("SOAK_SECS", 600)),
        keys: 20_000,
        value_bytes: 512,
        keys_per_txn: 16,
        workers: 4,
        sample_every: Duration::from_secs(1),
        bytes_per_sec: env_u64("SOAK_MIB_PER_SEC", 16) * MIB,
        max_journal_bytes: env_u64("SOAK_JOURNAL_MIB", 512) * MIB,
        memtable_bytes: env_u64("SOAK_MEMTABLE_MIB", 64) * MIB,
    })
    .await;
    assert_levels_off(&samples);
}
