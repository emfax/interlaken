//! Per-operator run-time metrics for `EXPLAIN ANALYZE` (ADR-0028).
//!
//! When a query runs with [`Metrics`], every operator's stream is wrapped to
//! count the batches and rows it produces and the time spent producing
//! them (including time in its inputs, as Postgres reports it). Scans add
//! their storage calls and the bytes of the values they read; blocking
//! operators record the peak memory they reserved. An operator that runs
//! more than once (inside an `Apply`) accumulates across runs, and `loops`
//! counts the runs.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use futures::Stream;
use futures::stream::StreamExt;

use crate::error::ExecError;
use crate::exec::BatchStream;

/// The counters of one operator, updated while it runs.
#[derive(Debug, Default)]
pub struct NodeMetrics {
    /// Times the operator was started.
    loops: AtomicU64,
    /// Rows produced.
    rows: AtomicU64,
    /// Non-empty batches produced.
    batches: AtomicU64,
    /// Nanoseconds spent producing output, including in inputs.
    elapsed_nanos: AtomicU64,
    /// Storage calls made (scans only).
    storage_calls: AtomicU64,
    /// Bytes of the values read from storage (scans only).
    bytes_read: AtomicU64,
    /// The most memory reserved at once (blocking operators only).
    peak_memory: AtomicUsize,
}

impl NodeMetrics {
    /// Record that the operator now holds `bytes` of memory.
    pub(crate) fn record_memory(&self, bytes: usize) {
        self.peak_memory.fetch_max(bytes, Ordering::Relaxed);
    }

    /// Record one storage call that read `bytes` of values.
    pub(crate) fn record_storage_call(&self, bytes: usize) {
        self.storage_calls.fetch_add(1, Ordering::Relaxed);
        self.bytes_read
            .fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Return the counters' current values.
    #[must_use]
    pub fn snapshot(&self) -> NodeStats {
        NodeStats {
            loops: self.loops.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
            elapsed: Duration::from_nanos(self.elapsed_nanos.load(Ordering::Relaxed)),
            storage_calls: self.storage_calls.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            peak_memory: self.peak_memory.load(Ordering::Relaxed),
        }
    }
}

/// A snapshot of one operator's metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeStats {
    /// Times the operator was started.
    pub loops: u64,
    /// Rows produced.
    pub rows: u64,
    /// Non-empty batches produced.
    pub batches: u64,
    /// Time spent producing output, including in inputs.
    pub elapsed: Duration,
    /// Storage calls made.
    pub storage_calls: u64,
    /// Bytes of the values read from storage.
    pub bytes_read: u64,
    /// The most memory reserved at once.
    pub peak_memory: usize,
}

/// The metrics of every operator of one plan, indexed by [`Physical::id`](crate::Physical::id).
#[derive(Debug)]
pub struct Metrics {
    /// One entry per operator.
    nodes: Vec<Arc<NodeMetrics>>,
}

impl Metrics {
    /// Construct empty metrics for a plan of `operators` operators.
    #[must_use]
    pub fn new(operators: usize) -> Arc<Self> {
        Arc::new(Self {
            nodes: (0..operators).map(|_| Arc::default()).collect(),
        })
    }

    /// Return the counters of operator `id`, if the plan has it.
    #[must_use]
    pub fn node(&self, id: usize) -> Option<&Arc<NodeMetrics>> {
        self.nodes.get(id)
    }
}

/// A stream that counts what its inner stream produces and how long it takes.
struct Metered {
    /// The operator's stream.
    inner: BatchStream,
    /// Where to count.
    metrics: Arc<NodeMetrics>,
}

impl Stream for Metered {
    type Item = Result<RecordBatch, ExecError>;

    /// Poll the inner stream, timing the poll and counting a produced batch.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let start = Instant::now();
        let poll = self.inner.poll_next_unpin(cx);
        let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.metrics
            .elapsed_nanos
            .fetch_add(nanos, Ordering::Relaxed);
        if let Poll::Ready(Some(Ok(batch))) = &poll
            && batch.num_rows() > 0
        {
            self.metrics.batches.fetch_add(1, Ordering::Relaxed);
            self.metrics
                .rows
                .fetch_add(batch.num_rows() as u64, Ordering::Relaxed);
        }
        poll
    }
}

/// Wrap an operator's stream so it reports into `metrics`, counting one more run.
pub(crate) fn metered(inner: BatchStream, metrics: Arc<NodeMetrics>) -> BatchStream {
    metrics.loops.fetch_add(1, Ordering::Relaxed);
    Metered { inner, metrics }.boxed()
}
