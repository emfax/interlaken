//! The transport's metrics (ADR-0039).
//!
//! See [`interlaken_storage::telemetry`] for how metrics are declared and
//! recorded. Calls this replica makes are counted per peer; requests it
//! answers are counted in total, since the server doesn't know which peer
//! sent them.

use interlaken_storage::telemetry::{Metric, MetricKind, MetricUnit};

/// Bytes of calls this replica makes, by peer and direction.
pub(crate) const BYTES: &str = "interlaken_net_bytes_total";
/// Calls this replica makes, by peer, kind and outcome.
pub(crate) const REQUESTS: &str = "interlaken_net_requests_total";
/// Bytes of requests this replica answers, by direction.
pub(crate) const SERVED_BYTES: &str = "interlaken_net_served_bytes_total";
/// Requests this replica answers, by kind.
pub(crate) const SERVED: &str = "interlaken_net_served_total";

/// Every metric the transport records.
pub const METRICS: &[Metric] = &[
    Metric {
        name: BYTES,
        kind: MetricKind::Counter,
        unit: MetricUnit::Bytes,
        labels: &["peer", "direction"],
        help: "Bytes of the calls this replica makes to each peer: requests `sent` and \
               responses `received`, as encoded, without gRPC and TLS framing. Divided by \
               bytes committed, the network cost of each committed byte.",
    },
    Metric {
        name: REQUESTS,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["peer", "kind", "outcome"],
        help: "Calls this replica makes to each peer, by kind (`append`, `vote`, `pre_vote`, \
               `commit`, `read_index`, `snapshot_chunk`, `transfer_leader`, `membership`) and \
               outcome (`ok`; `not_delivered`, surely not received; `no_reply`, perhaps \
               received).",
    },
    Metric {
        name: SERVED_BYTES,
        kind: MetricKind::Counter,
        unit: MetricUnit::Bytes,
        labels: &["direction"],
        help: "Bytes of the requests this replica answers: requests `received` and responses \
               `sent`, as encoded.",
    },
    Metric {
        name: SERVED,
        kind: MetricKind::Counter,
        unit: MetricUnit::Count,
        labels: &["kind"],
        help: "Requests this replica answered, by kind.",
    },
];
