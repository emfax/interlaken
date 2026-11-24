//! Metric declarations and the storage layer's metrics (ADR-0039).
//!
//! Interlaken records metrics through the [`metrics`] facade and spans
//! through [`tracing`], and never installs a recorder or subscriber: the
//! program embedding it chooses an exporter. With none installed, recording
//! is a check of a global and costs almost nothing.
//!
//! Every metric a crate records is declared once in that crate's `METRICS`
//! list as a [`Metric`], with its kind, unit, labels and description.
//! [`describe`] passes the declarations to the installed recorder, and the
//! book's metric reference is generated from them.
//!
//! Totals of time are gauges that only rise, since `metrics` counters
//! count whole numbers; their names end in `_seconds`, not `_total`, and
//! `rate()` works on them as on counters.
//!
//! Labels hold only values from fixed sets (or node IDs, or attribution
//! keys an operator has opted into): never keys, values, SQL text or table
//! names, which policies may hide.

use std::time::Duration;

/// What kind of series a metric is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    /// A cumulative count that only rises.
    Counter,
    /// A value sampled at a point in time.
    Gauge,
    /// A distribution of observed values.
    Histogram,
}

/// A metric's unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricUnit {
    /// A count of things.
    Count,
    /// Seconds.
    Seconds,
    /// Bytes.
    Bytes,
}

/// The declaration of one metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metric {
    /// Its name, `interlaken_<area>_<what>_<unit>`.
    pub name: &'static str,
    /// What kind of series it is.
    pub kind: MetricKind,
    /// Its unit.
    pub unit: MetricUnit,
    /// The labels it carries, each from a fixed set of values.
    pub labels: &'static [&'static str],
    /// What it measures.
    pub help: &'static str,
}

/// Pass the declarations of `metrics` to the installed recorder, so an
/// exporter can show their units and descriptions. Does nothing if no
/// recorder is installed.
pub fn describe(metrics: &[Metric]) {
    for metric in metrics {
        let unit = match metric.unit {
            MetricUnit::Count => metrics::Unit::Count,
            MetricUnit::Seconds => metrics::Unit::Seconds,
            MetricUnit::Bytes => metrics::Unit::Bytes,
        };
        match metric.kind {
            MetricKind::Counter => metrics::describe_counter!(metric.name, unit, metric.help),
            MetricKind::Gauge => metrics::describe_gauge!(metric.name, unit, metric.help),
            MetricKind::Histogram => metrics::describe_histogram!(metric.name, unit, metric.help),
        }
    }
}

/// Transactions that asked to commit, by isolation level and outcome.
pub(crate) const COMMITS: &str = "interlaken_storage_commits_total";
/// From a commit request to its answer.
pub(crate) const COMMIT_DURATION: &str = "interlaken_storage_commit_duration_seconds";
/// Key and value bytes in committed write sets.
pub(crate) const BYTES_COMMITTED: &str = "interlaken_storage_bytes_committed_total";
/// Transactions per commit group.
pub(crate) const COMMIT_GROUP_SIZE: &str = "interlaken_storage_commit_group_size";
/// Commit sequencer time, by phase.
pub(crate) const SEQUENCER_BUSY: &str = "interlaken_storage_sequencer_busy_seconds";
/// Garbage collection: versions examined and removed.
pub(crate) const GC_VERSIONS: &str = "interlaken_storage_gc_versions_total";
/// Garbage collection passes, by outcome.
pub(crate) const GC_PASSES: &str = "interlaken_storage_gc_passes_total";
/// Time writers waited for memtables to flush.
pub(crate) const WRITE_THROTTLE: &str = "interlaken_storage_write_throttle_seconds";
/// Bytes on disk, by kind.
pub(crate) const DISK_BYTES: &str = "interlaken_storage_disk_bytes";
/// Bytes the process wrote to disk.
pub(crate) const BYTES_WRITTEN: &str = "interlaken_storage_bytes_written_total";
/// fjall's memtables, active and awaiting flush.
pub(crate) const MEMTABLE_BYTES: &str = "interlaken_storage_memtable_bytes";
/// Memtable flushes queued.
pub(crate) const FLUSH_BACKLOG: &str = "interlaken_storage_flush_backlog";
/// Tables in level 0 of the data keyspace.
pub(crate) const L0_TABLES: &str = "interlaken_storage_l0_tables";
/// Time fjall has spent compacting.
pub(crate) const COMPACTION: &str = "interlaken_storage_compaction_seconds";
/// Open transactions within the maximum age.
pub(crate) const ACTIVE_TRANSACTIONS: &str = "interlaken_storage_active_transactions";
/// Transactions that exceeded the maximum age.
pub(crate) const EXPIRED_TRANSACTIONS: &str = "interlaken_storage_expired_transactions_total";
/// Age of the oldest open transaction's snapshot.
pub(crate) const OLDEST_SNAPSHOT_AGE: &str = "interlaken_storage_oldest_snapshot_age_seconds";

/// Every metric the storage layer records.
pub const METRICS: &[Metric] = &[
    Metric {
        name: COMMITS,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["isolation", "outcome"],
        help: "Transactions with writes that asked to commit, by isolation level and outcome \
               (`committed`, `conflict`, `error`).",
    },
    Metric {
        name: COMMIT_DURATION,
        kind: MetricKind::Histogram,
        unit: MetricUnit::Seconds,
        labels: &["durability"],
        help: "Time from a commit request to its answer.",
    },
    Metric {
        name: BYTES_COMMITTED,
        kind: MetricKind::Counter,
        unit: MetricUnit::Bytes,
        labels: &[],
        help: "Key and value bytes in committed write sets.",
    },
    Metric {
        name: COMMIT_GROUP_SIZE,
        kind: MetricKind::Histogram,
        unit: MetricUnit::Count,
        labels: &[],
        help: "Transactions in each group the commit sequencer writes.",
    },
    Metric {
        name: SEQUENCER_BUSY,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Seconds,
        labels: &["phase"],
        help: "Cumulative commit sequencer time, by phase (`validation`, `write`). Near one second per \
               second means the sequencer limits commit throughput.",
    },
    Metric {
        name: GC_VERSIONS,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["outcome"],
        help: "Versions garbage collection examined and removed (`examined`, `removed`).",
    },
    Metric {
        name: GC_PASSES,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["outcome"],
        help: "Garbage collection passes, by outcome (`completed`, `failed`).",
    },
    Metric {
        name: WRITE_THROTTLE,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Seconds,
        labels: &[],
        help: "Cumulative time snapshot installs waited for memtables to flush.",
    },
    Metric {
        name: DISK_BYTES,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Bytes,
        labels: &["kind"],
        help: "Bytes on disk, by kind (`data`, `journal`, `other`). Sampled.",
    },
    Metric {
        name: BYTES_WRITTEN,
        kind: MetricKind::Counter,
        unit: MetricUnit::Bytes,
        labels: &[],
        help: "Bytes the process has written to disk, from `/proc/self/io` (Linux only). \
               Divided by bytes committed, the write amplification. Sampled.",
    },
    Metric {
        name: MEMTABLE_BYTES,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Bytes,
        labels: &[],
        help: "Bytes in fjall's memtables, active and awaiting flush. Sampled.",
    },
    Metric {
        name: FLUSH_BACKLOG,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Count,
        labels: &[],
        help: "Memtable flushes queued. Sampled.",
    },
    Metric {
        name: L0_TABLES,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Count,
        labels: &[],
        help: "Tables in level 0 of the data keyspace; a rising count means compaction is \
               behind. Sampled.",
    },
    Metric {
        name: COMPACTION,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Seconds,
        labels: &[],
        help: "Cumulative time fjall has spent compacting. Sampled.",
    },
    Metric {
        name: ACTIVE_TRANSACTIONS,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Count,
        labels: &[],
        help: "Open transactions within the maximum age. Sampled.",
    },
    Metric {
        name: EXPIRED_TRANSACTIONS,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &[],
        help: "Transactions that exceeded the maximum transaction age. Sampled.",
    },
    Metric {
        name: OLDEST_SNAPSHOT_AGE,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Seconds,
        labels: &[],
        help: "Age of the oldest open transaction's snapshot, which holds back garbage \
               collection. Sampled.",
    },
];

/// Convert a duration to seconds for a metric.
pub(crate) fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

/// Convert a count to a metric value, which is a float.
#[expect(clippy::cast_precision_loss, reason = "metrics are approximate")]
pub(crate) fn float(n: u64) -> f64 {
    n as f64
}
