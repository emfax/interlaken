//! The metrics sampler (ADR-0039): a background thread that records the
//! storage layer's sizes, backlogs and transaction counts as gauges every
//! [`StorageOptions::metrics_interval`](crate::StorageOptions::metrics_interval).
//!
//! Counters that change with each operation are recorded where the
//! operation happens; the sampler covers what is cheaper to read now and
//! then than to track as it changes.

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::commit::CommitClock;
use crate::telemetry;
use crate::{Engine, Error, Result};

/// Record one sample of `engine` and `clock` as gauges.
///
/// # Errors
///
/// Returns a storage error if fjall's sizes can't be read.
pub(crate) fn record(engine: &Engine, clock: &CommitClock) -> Result<()> {
    let sample = engine.sample()?;
    let disk = |kind: &'static str, bytes: u64| {
        metrics::gauge!(telemetry::DISK_BYTES, "kind" => kind).set(telemetry::float(bytes));
    };
    disk("data", sample.data_bytes);
    disk("journal", sample.journal_bytes);
    disk("other", sample.other_bytes);
    metrics::gauge!(telemetry::MEMTABLE_BYTES).set(telemetry::float(sample.memtable_bytes));
    metrics::gauge!(telemetry::FLUSH_BACKLOG).set(telemetry::float(sample.flush_backlog as u64));
    metrics::gauge!(telemetry::L0_TABLES).set(telemetry::float(sample.l0_tables as u64));
    metrics::gauge!(telemetry::COMPACTION).set(telemetry::seconds(sample.compacting));
    let (active, _) = clock.active();
    metrics::gauge!(telemetry::ACTIVE_TRANSACTIONS).set(telemetry::float(active as u64));
    metrics::counter!(telemetry::EXPIRED_TRANSACTIONS).absolute(clock.expired_count());
    metrics::gauge!(telemetry::OLDEST_SNAPSHOT_AGE)
        .set(clock.oldest_pin_age().map_or(0.0, telemetry::seconds));
    if let Some(written) = process_bytes_written() {
        metrics::counter!(telemetry::BYTES_WRITTEN).absolute(written);
    }
    Ok(())
}

/// Return the bytes this process has caused to be written to storage, from
/// `/proc/self/io`, or `None` where that isn't available.
fn process_bytes_written() -> Option<u64> {
    let io = std::fs::read_to_string("/proc/self/io").ok()?;
    io.lines()
        .find_map(|line| line.strip_prefix("write_bytes:"))
        .and_then(|value| value.trim().parse().ok())
}

/// The sampler thread. Dropping it stops and joins the thread.
#[derive(Debug)]
pub(crate) struct Sampler {
    stop: Option<flume::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Sampler {
    /// Start a thread that records a sample every `interval`.
    pub(crate) fn start(
        engine: Engine,
        clock: Arc<CommitClock>,
        interval: Duration,
    ) -> Result<Self> {
        let (stop, stopped) = flume::bounded::<()>(1);
        let thread = thread::Builder::new()
            .name("interlaken-metrics".to_owned())
            .spawn(move || {
                while let Err(flume::RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                    // A failed sample leaves the last values; the next may succeed.
                    let _ = record(&engine, &clock);
                }
            })
            .map_err(|_| Error::PoolUnavailable("failed to spawn the metrics sampler"))?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for Sampler {
    /// Signal the thread to stop and join it.
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != thread::current().id()
        {
            let _ = thread.join();
        }
    }
}
