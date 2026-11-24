//! The engine's metrics (ADR-0039), and describing every crate's.
//!
//! See [`interlaken_storage::telemetry`] for how metrics are declared and
//! recorded. The engine records statements, their usage, memory, security
//! denials and policy proposals.

use interlaken_storage::telemetry::{Metric, MetricKind, MetricUnit};

/// Statements run, by kind and outcome.
pub(crate) const STATEMENTS: &str = "interlaken_statements_total";
/// Time per statement phase.
pub(crate) const STATEMENT_DURATION: &str = "interlaken_statement_duration_seconds";
/// Executor time per statement.
pub(crate) const STATEMENT_CPU: &str = "interlaken_statement_cpu_seconds";
/// Rows storage returned to scans.
pub(crate) const ROWS_SCANNED: &str = "interlaken_rows_scanned_total";
/// Rows statements returned.
pub(crate) const ROWS_RETURNED: &str = "interlaken_rows_returned_total";
/// Bytes scans read.
pub(crate) const BYTES_READ: &str = "interlaken_bytes_read_total";
/// Bytes statements added to write sets.
pub(crate) const BYTES_WRITTEN: &str = "interlaken_bytes_written_total";
/// Peak query memory per statement.
pub(crate) const QUERY_MEMORY_PEAK: &str = "interlaken_query_memory_peak_bytes";
/// Statements refused for their memory budget.
pub(crate) const QUERY_MEMORY_EXCEEDED: &str = "interlaken_query_memory_exceeded_total";
/// Statements refused by security, by kind.
pub(crate) const AUTHZ_DENIALS: &str = "interlaken_authz_denials_total";
/// Policy proposal steps, by step and outcome.
pub(crate) const POLICY_PROPOSALS: &str = "interlaken_policy_proposals_total";

/// Every metric the engine records.
pub const METRICS: &[Metric] = &[
    Metric {
        name: STATEMENTS,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["kind", "outcome"],
        help: "Statements run, by kind (`select`, `insert`, `update`, `delete`, `unplanned` for a \
               query that failed before planning finished, `ddl`, `policy`, `admin`, `other`) \
               and outcome (`ok`, `error`, `cancelled`).",
    },
    Metric {
        name: STATEMENT_DURATION,
        kind: MetricKind::Histogram,
        unit: MetricUnit::Seconds,
        labels: &["kind", "phase"],
        help: "Time per statement phase (`parse`, `plan`, `execute`). A streamed statement's \
               execution includes time the caller spends between batches.",
    },
    Metric {
        name: STATEMENT_CPU,
        kind: MetricKind::Histogram,
        unit: MetricUnit::Seconds,
        labels: &["kind"],
        help: "Time each query or DML statement spent executing on its task, not counting \
               waits for storage.",
    },
    Metric {
        name: ROWS_SCANNED,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &[],
        help: "Rows storage returned to scans and lookups.",
    },
    Metric {
        name: ROWS_RETURNED,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &[],
        help: "Rows statements returned. Divided by rows scanned, how much scanning was useful.",
    },
    Metric {
        name: BYTES_READ,
        kind: MetricKind::Counter,
        unit: MetricUnit::Bytes,
        labels: &[],
        help: "Bytes of the values scans and lookups read, as decoded.",
    },
    Metric {
        name: BYTES_WRITTEN,
        kind: MetricKind::Counter,
        unit: MetricUnit::Bytes,
        labels: &[],
        help: "Key and value bytes statements added to their transactions' write sets.",
    },
    Metric {
        name: QUERY_MEMORY_PEAK,
        kind: MetricKind::Histogram,
        unit: MetricUnit::Bytes,
        labels: &[],
        help: "The most query memory each query or DML statement reserved at once.",
    },
    Metric {
        name: QUERY_MEMORY_EXCEEDED,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &[],
        help: "Statements that failed for exceeding a memory budget (SQLSTATE `53200`).",
    },
    Metric {
        name: AUTHZ_DENIALS,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["kind"],
        help: "Statements refused by security, by kind: `column` (a column policy), `write` (a \
               write check), `statement` (a missing privilege or another security error). Rows \
               policies filter out aren't denials.",
    },
    Metric {
        name: POLICY_PROPOSALS,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["step", "outcome"],
        help: "Policy proposal steps (`submit`, `verify`, `activate`) by outcome (`ok`, `error`).",
    },
];

/// Pass every metric declaration of the crates this build includes to the
/// installed recorder, so an exporter can show their units and
/// descriptions. Call it once, after installing a recorder.
///
/// # Examples
///
/// ```
/// // With no recorder installed, this does nothing.
/// interlaken_engine::describe_metrics();
/// ```
pub fn describe_metrics() {
    interlaken_storage::telemetry::describe(interlaken_storage::telemetry::METRICS);
    interlaken_storage::telemetry::describe(METRICS);
    #[cfg(feature = "cluster")]
    {
        interlaken_storage::telemetry::describe(interlaken_replication::telemetry::METRICS);
        interlaken_storage::telemetry::describe(interlaken_net::telemetry::METRICS);
    }
}

/// Return the security denial kind of `error`, if it is one.
pub(crate) fn denial(error: &crate::EngineError) -> Option<&'static str> {
    use crate::EngineError as E;
    use interlaken_ir::security::SecurityError;
    match error {
        E::Security(SecurityError::ColumnDenied { .. }) => Some("column"),
        E::Exec(interlaken_exec::ExecError::PermissionDenied { .. }) => Some("write"),
        E::Security(_) | E::PermissionDenied(_) => Some("statement"),
        E::Verify(_) if error.sqlstate() == "42501" => Some("statement"),
        _ => None,
    }
}

/// Record the metrics every failed statement shares: denials and memory.
pub(crate) fn record_failure(error: &crate::EngineError) {
    if let Some(kind) = denial(error) {
        metrics::counter!(AUTHZ_DENIALS, "kind" => kind).increment(1);
    }
    if error.sqlstate() == "53200" {
        metrics::counter!(QUERY_MEMORY_EXCEEDED).increment(1);
    }
}
