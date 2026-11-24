//! Replication's metrics (ADR-0039).
//!
//! See [`interlaken_storage::telemetry`] for how metrics are declared and
//! recorded. Raft's state is recorded by a task that follows openraft's
//! metrics watch; proposals, reads and snapshot transfers where they happen.
//! Raft's gauges carry the `node` label, so several replicas in one process
//! (as in tests) don't overwrite each other.

use interlaken_storage::telemetry::{Metric, MetricKind, MetricUnit};
use openraft::async_runtime::WatchReceiver;
use openraft::{LogId, ServerState};

use crate::types::{NodeId, Raft};

/// The current term.
pub(crate) const TERM: &str = "interlaken_raft_term";
/// 1 on the leader.
pub(crate) const IS_LEADER: &str = "interlaken_raft_is_leader";
/// Leader changes seen.
pub(crate) const LEADER_CHANGES: &str = "interlaken_raft_leader_changes_total";
/// The last committed log index.
pub(crate) const COMMIT_INDEX: &str = "interlaken_raft_commit_index";
/// The last applied log index.
pub(crate) const APPLIED_INDEX: &str = "interlaken_raft_applied_index";
/// Entries each follower trails the leader by.
pub(crate) const FOLLOWER_LAG: &str = "interlaken_raft_follower_lag_entries";
/// From proposal to commit.
pub(crate) const PROPOSAL_DURATION: &str = "interlaken_raft_proposal_duration_seconds";
/// Transactions per proposal.
pub(crate) const PROPOSAL_SIZE: &str = "interlaken_raft_proposal_size";
/// Fresh reads, by path.
pub(crate) const READS: &str = "interlaken_raft_reads_total";
/// Snapshot transfers.
pub(crate) const SNAPSHOTS: &str = "interlaken_raft_snapshots_total";
/// Snapshot bytes moved.
pub(crate) const SNAPSHOT_BYTES: &str = "interlaken_raft_snapshot_bytes_total";
/// Snapshot transfer time.
pub(crate) const SNAPSHOT_DURATION: &str = "interlaken_raft_snapshot_duration_seconds";

/// Every metric replication records.
pub const METRICS: &[Metric] = &[
    Metric {
        name: TERM,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Count,
        labels: &["node"],
        help: "The current Raft term.",
    },
    Metric {
        name: IS_LEADER,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Count,
        labels: &["node"],
        help: "1 on the leader, 0 elsewhere.",
    },
    Metric {
        name: LEADER_CHANGES,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["node"],
        help: "Times this replica saw a new leader.",
    },
    Metric {
        name: COMMIT_INDEX,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Count,
        labels: &["node"],
        help: "The index of the last log entry known committed.",
    },
    Metric {
        name: APPLIED_INDEX,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Count,
        labels: &["node"],
        help: "The index of the last log entry applied here.",
    },
    Metric {
        name: FOLLOWER_LAG,
        kind: MetricKind::Gauge,
        unit: MetricUnit::Count,
        labels: &["node", "peer"],
        help: "On the leader: how many entries each other member's log trails the leader's by.",
    },
    Metric {
        name: PROPOSAL_DURATION,
        kind: MetricKind::Histogram,
        unit: MetricUnit::Seconds,
        labels: &[],
        help: "On the leader: time from proposing a group of commits to Raft until it is \
               committed and applied.",
    },
    Metric {
        name: PROPOSAL_SIZE,
        kind: MetricKind::Histogram,
        unit: MetricUnit::Count,
        labels: &[],
        help: "On the leader: transactions in each proposal.",
    },
    Metric {
        name: READS,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["kind"],
        help: "Fresh-read requests answered, by path: `lease` or `read_index` on the leader, \
               `forwarded` from a follower to the leader.",
    },
    Metric {
        name: SNAPSHOTS,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["direction", "outcome"],
        help: "Snapshot transfers, by direction (`sent`, `received`) and outcome (`ok`, `error`).",
    },
    Metric {
        name: SNAPSHOT_BYTES,
        kind: MetricKind::Counter,
        unit: MetricUnit::Bytes,
        labels: &["direction"],
        help: "Snapshot bytes sent and received.",
    },
    Metric {
        name: SNAPSHOT_DURATION,
        kind: MetricKind::Histogram,
        unit: MetricUnit::Seconds,
        labels: &["direction"],
        help: "Time per snapshot transfer, from its first chunk to the end of sending, or of \
               installing.",
    },
];

/// Convert a count to a gauge value.
#[expect(clippy::cast_precision_loss, reason = "metrics are approximate")]
fn float(n: u64) -> f64 {
    n as f64
}

/// Record Raft's state as gauges each time it changes, until Raft stops.
pub(crate) async fn follow(id: NodeId, raft: Raft) {
    let node = id.to_string();
    let mut watch = raft.metrics();
    let mut leader = None;
    loop {
        let metrics = watch.borrow_watched().clone();
        let gauge = |name: &'static str, value: f64| {
            metrics::gauge!(name, "node" => node.clone()).set(value);
        };
        gauge(TERM, float(metrics.current_term));
        gauge(
            IS_LEADER,
            if metrics.state == ServerState::Leader {
                1.0
            } else {
                0.0
            },
        );
        if let Some(index) = metrics.local_committed.as_ref().map(LogId::index) {
            gauge(COMMIT_INDEX, float(index));
        }
        if let Some(index) = metrics.last_applied.as_ref().map(LogId::index) {
            gauge(APPLIED_INDEX, float(index));
        }
        if metrics.current_leader.is_some() && metrics.current_leader != leader {
            metrics::counter!(LEADER_CHANGES, "node" => node.clone()).increment(1);
        }
        leader = metrics.current_leader;
        if let (Some(replication), Some(last)) = (&metrics.replication, metrics.last_log_index) {
            for (peer, matched) in replication.iter().filter(|(peer, _)| **peer != id) {
                if let Some(matched) = matched.as_ref().map(LogId::index) {
                    metrics::gauge!(FOLLOWER_LAG, "node" => node.clone(), "peer" => peer.to_string())
                        .set(float(last.saturating_sub(matched)));
                }
            }
        }
        if watch.changed().await.is_err() {
            return;
        }
    }
}
