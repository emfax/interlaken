//! What recording metrics costs commit throughput (ADR-0039 phase 5).
//!
//! Concurrent clients commit blind writes for a fixed time, in rounds that
//! alternate between no recorder and a Prometheus recorder. A recorder can
//! only be installed once per process, so the one installed here forwards
//! to Prometheus or discards, switched between rounds. The test fails if
//! the median throughput with Prometheus is more than 5% below the median
//! without.
//!
//! Ignored by default; nightly CI runs it in release:
//!
//! ```sh
//! cargo test --release -p interlaken-storage --test instrumentation_cost -- --ignored --nocapture
//! ```
//!
//! `COST_ROUND_SECS` sets each round's length (default 2), and
//! `COST_ROUNDS` the rounds of each kind (default 5).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use interlaken_storage::{
    Durability, FjallStorage, Isolation, Storage, StorageOptions, TxnOptions,
};
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};

/// A recorder that forwards to `inner` while `on`, and discards otherwise.
struct Switch<R> {
    /// Whether to forward.
    on: Arc<AtomicBool>,
    /// The recorder forwarded to.
    inner: R,
}

impl<R: Recorder> Recorder for Switch<R> {
    /// Forward the description.
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.inner.describe_counter(key, unit, description);
    }

    /// Forward the description.
    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.inner.describe_gauge(key, unit, description);
    }

    /// Forward the description.
    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.inner.describe_histogram(key, unit, description);
    }

    /// Return the inner counter while on, else one that discards.
    fn register_counter(&self, key: &Key, metadata: &Metadata<'_>) -> Counter {
        if self.on.load(Ordering::Relaxed) {
            self.inner.register_counter(key, metadata)
        } else {
            Counter::noop()
        }
    }

    /// Return the inner gauge while on, else one that discards.
    fn register_gauge(&self, key: &Key, metadata: &Metadata<'_>) -> Gauge {
        if self.on.load(Ordering::Relaxed) {
            self.inner.register_gauge(key, metadata)
        } else {
            Gauge::noop()
        }
    }

    /// Return the inner histogram while on, else one that discards.
    fn register_histogram(&self, key: &Key, metadata: &Metadata<'_>) -> Histogram {
        if self.on.load(Ordering::Relaxed) {
            self.inner.register_histogram(key, metadata)
        } else {
            Histogram::noop()
        }
    }
}

/// Return the environment variable `name` parsed, or `default`.
fn setting<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Commit blind writes from `clients` clients for `secs`, returning commits per second.
async fn round(storage: &FjallStorage, clients: usize, secs: f64, next: &Arc<AtomicU64>) -> f64 {
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    let tasks: Vec<_> = (0..clients)
        .map(|_| {
            let (storage, next) = (storage.clone(), Arc::clone(next));
            tokio::spawn(async move {
                let mut commits = 0u64;
                while Instant::now() < deadline {
                    let key = next.fetch_add(1, Ordering::Relaxed);
                    let txn = storage
                        .begin(TxnOptions::new(Isolation::RepeatableRead))
                        .await
                        .unwrap();
                    txn.put(
                        Bytes::from(key.to_be_bytes().to_vec()),
                        Bytes::from(vec![7; 100]),
                    )
                    .unwrap();
                    txn.commit().await.unwrap();
                    commits += 1;
                }
                commits
            })
        })
        .collect();
    let mut total = 0;
    for task in tasks {
        total += task.await.unwrap();
    }
    #[expect(clippy::cast_precision_loss, reason = "a rate")]
    let rate = total as f64 / secs;
    rate
}

/// Return the median of `values`.
fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measures throughput for minutes; run in release"]
/// Assert recording metrics with Prometheus costs commit throughput at most 5%.
async fn prometheus_costs_little() {
    let on = Arc::new(AtomicBool::new(false));
    let inner = interlaken_telemetry::prometheus_builder()
        .unwrap()
        .build_recorder();
    metrics::set_global_recorder(Switch {
        on: Arc::clone(&on),
        inner,
    })
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let options = StorageOptions {
        durability: Durability::Buffered,
        ..StorageOptions::default()
    };
    let storage = FjallStorage::open(dir.path(), options).unwrap();
    let (secs, rounds) = (setting("COST_ROUND_SECS", 2.0), setting("COST_ROUNDS", 5));
    let next = Arc::new(AtomicU64::new(0));
    // Warm up before measuring.
    round(&storage, 16, secs, &next).await;
    let (mut off, mut with) = (Vec::new(), Vec::new());
    for _ in 0..rounds {
        on.store(false, Ordering::Relaxed);
        off.push(round(&storage, 16, secs, &next).await);
        on.store(true, Ordering::Relaxed);
        with.push(round(&storage, 16, secs, &next).await);
    }
    let (off, with) = (median(off), median(with));
    let ratio = with / off;
    println!("commits/s without a recorder {off:.0}, with Prometheus {with:.0} ({ratio:.3})");
    assert!(
        ratio >= 0.95,
        "Prometheus cost {:.1}% of commit throughput",
        (1.0 - ratio) * 100.0
    );
}
