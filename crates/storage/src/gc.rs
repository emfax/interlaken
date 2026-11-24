//! The version GC task (ADR-0008).
//!
//! A pass takes the watermark from the commit clock (revoking pins past the
//! maximum transaction age), then walks `data` in chunks, each a
//! low-priority job on the storage pool, so at most one GC chunk is in
//! flight and reads always go first. At the end of a pass the watermark is
//! recorded in `meta`. A background thread runs a pass every
//! [`StorageOptions::gc_interval`](crate::StorageOptions::gc_interval);
//! [`FjallStorage::run_gc`](crate::FjallStorage::run_gc) runs one on demand.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tracing::Instrument;

use crate::commit::CommitClock;
use crate::engine::gc::{GcChunk, collect_chunk};
use crate::pool::StoragePool;
use crate::telemetry;
use crate::{Error, Result, Timestamp};

/// What one GC pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Versions no snapshot older than this could see were eligible.
    pub watermark: Timestamp,
    /// Versions looked at.
    pub examined: u64,
    /// Versions removed.
    pub removed: u64,
}

impl GcReport {
    /// Return the versions left after the pass.
    #[must_use]
    pub fn retained(&self) -> u64 {
        self.examined - self.removed
    }
}

/// Cumulative GC counters since the database was opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Completed passes.
    pub passes: u64,
    /// Passes abandoned because of a storage error.
    pub failed_passes: u64,
    /// Versions looked at, over all passes.
    pub versions_examined: u64,
    /// Versions removed, over all passes.
    pub versions_removed: u64,
    /// The most recent completed pass.
    pub last_pass: GcReport,
}

/// The live counters behind [`GcStats`].
#[derive(Debug, Default)]
pub(crate) struct GcCounters {
    passes: AtomicU64,
    failed_passes: AtomicU64,
    examined: AtomicU64,
    removed: AtomicU64,
    last_watermark: AtomicU64,
    last_examined: AtomicU64,
    last_removed: AtomicU64,
}

impl GcCounters {
    /// Record a completed pass.
    fn record(&self, report: &GcReport) {
        metrics::counter!(telemetry::GC_VERSIONS, "outcome" => "examined")
            .increment(report.examined);
        metrics::counter!(telemetry::GC_VERSIONS, "outcome" => "removed").increment(report.removed);
        metrics::counter!(telemetry::GC_PASSES, "outcome" => "completed").increment(1);
        self.examined.fetch_add(report.examined, Ordering::Relaxed);
        self.removed.fetch_add(report.removed, Ordering::Relaxed);
        self.last_watermark
            .store(report.watermark.into_raw(), Ordering::Relaxed);
        self.last_examined.store(report.examined, Ordering::Relaxed);
        self.last_removed.store(report.removed, Ordering::Relaxed);
        self.passes.fetch_add(1, Ordering::Relaxed);
    }

    /// Return a snapshot of the counters.
    pub(crate) fn snapshot(&self) -> GcStats {
        GcStats {
            passes: self.passes.load(Ordering::Relaxed),
            failed_passes: self.failed_passes.load(Ordering::Relaxed),
            versions_examined: self.examined.load(Ordering::Relaxed),
            versions_removed: self.removed.load(Ordering::Relaxed),
            last_pass: GcReport {
                watermark: Timestamp::from_raw(self.last_watermark.load(Ordering::Relaxed)),
                examined: self.last_examined.load(Ordering::Relaxed),
                removed: self.last_removed.load(Ordering::Relaxed),
            },
        }
    }
}

/// What a pass needs.
#[derive(Clone)]
pub(crate) struct GcContext {
    pub(crate) pool: Arc<StoragePool>,
    pub(crate) clock: Arc<CommitClock>,
    pub(crate) counters: Arc<GcCounters>,
    /// Versions examined per chunk.
    pub(crate) chunk_versions: usize,
}

impl GcContext {
    /// Run one pass from an async caller.
    pub(crate) async fn pass(&self) -> Result<GcReport> {
        let span = tracing::info_span!(
            "interlaken.gc.pass",
            examined = tracing::field::Empty,
            removed = tracing::field::Empty,
        );
        let report = self.pass_async().instrument(span.clone()).await?;
        span.record("examined", report.examined);
        span.record("removed", report.removed);
        Ok(report)
    }

    /// Run one pass from an async caller, within its span.
    async fn pass_async(&self) -> Result<GcReport> {
        let watermark = self.clock.horizon();
        let mut report = GcReport {
            watermark,
            ..GcReport::default()
        };
        let mut start: Option<Vec<u8>> = None;
        loop {
            let chunk_versions = self.chunk_versions;
            let from = start.take();
            let chunk = self
                .pool
                .run_low(move |engine| {
                    collect_chunk(engine, from.as_deref(), watermark, chunk_versions)
                })
                .await??;
            match self.absorb(&mut report, chunk) {
                Some(resume) => start = Some(resume),
                None => break,
            }
        }
        self.pool
            .run_low(move |engine| engine.set_gc_watermark(watermark))
            .await??;
        self.counters.record(&report);
        Ok(report)
    }

    /// Run one pass from the GC thread, stopping early if `stop` closes.
    fn pass_blocking(&self, stop: &flume::Receiver<()>) -> Result<Option<GcReport>> {
        let span = tracing::info_span!(
            "interlaken.gc.pass",
            examined = tracing::field::Empty,
            removed = tracing::field::Empty,
        );
        let _entered = span.enter();
        let report = self.pass_blocking_inner(stop)?;
        if let Some(report) = &report {
            span.record("examined", report.examined);
            span.record("removed", report.removed);
        }
        Ok(report)
    }

    /// Run one pass from the GC thread, within its span.
    fn pass_blocking_inner(&self, stop: &flume::Receiver<()>) -> Result<Option<GcReport>> {
        let watermark = self.clock.horizon();
        let mut report = GcReport {
            watermark,
            ..GcReport::default()
        };
        let mut start: Option<Vec<u8>> = None;
        loop {
            if stop.is_disconnected() {
                return Ok(None);
            }
            let chunk_versions = self.chunk_versions;
            let from = start.take();
            let chunk = self.pool.run_low_blocking(move |engine| {
                collect_chunk(engine, from.as_deref(), watermark, chunk_versions)
            })??;
            match self.absorb(&mut report, chunk) {
                Some(resume) => start = Some(resume),
                None => break,
            }
        }
        self.pool
            .run_low_blocking(move |engine| engine.set_gc_watermark(watermark))??;
        self.counters.record(&report);
        Ok(Some(report))
    }

    /// Add a chunk's counts to `report` and return where the next chunk starts.
    fn absorb(&self, report: &mut GcReport, chunk: GcChunk) -> Option<Vec<u8>> {
        report.examined += chunk.examined;
        report.removed += chunk.removed;
        chunk.resume
    }
}

/// The background GC thread. Dropping it stops and joins the thread.
#[derive(Debug)]
pub(crate) struct GcTask {
    stop: Option<flume::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl GcTask {
    /// Start a thread that runs a pass every `interval`.
    pub(crate) fn start(context: GcContext, interval: Duration) -> Result<Self> {
        let (stop, stopped) = flume::bounded::<()>(1);
        let thread = thread::Builder::new()
            .name("interlaken-gc".to_owned())
            .spawn(move || {
                while let Err(flume::RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                    match context.pass_blocking(&stopped) {
                        Ok(Some(_)) => {}
                        Ok(None) => return,
                        Err(error) => {
                            context
                                .counters
                                .failed_passes
                                .fetch_add(1, Ordering::Relaxed);
                            metrics::counter!(telemetry::GC_PASSES, "outcome" => "failed")
                                .increment(1);
                            tracing::warn!(%error, "garbage collection pass failed");
                        }
                    }
                }
            })
            .map_err(|_| Error::PoolUnavailable("failed to spawn the GC thread"))?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for GcTask {
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
