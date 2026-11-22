//! Counters of a replica's network traffic.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use interlaken_replication::types::NodeId;

/// Requests sent to one peer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerTraffic {
    /// Requests sent.
    pub sent: u64,
    /// Requests that got no answer.
    pub failed: u64,
}

/// A snapshot of a replica's network counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetMetrics {
    /// Requests this replica answered.
    pub served: u64,
    /// Requests sent to each peer.
    pub peers: BTreeMap<NodeId, PeerTraffic>,
}

/// The live counters.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// Requests answered.
    served: AtomicU64,
    /// Requests sent, by peer.
    peers: Mutex<BTreeMap<NodeId, PeerTraffic>>,
}

impl Counters {
    /// Count a request answered.
    pub(crate) fn served(&self) {
        self.served.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a request sent to `peer`, and whether it failed.
    pub(crate) fn sent(&self, peer: NodeId, failed: bool) {
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let traffic = peers.entry(peer).or_default();
        traffic.sent += 1;
        traffic.failed += u64::from(failed);
    }

    /// Return the counters' current values.
    pub(crate) fn snapshot(&self) -> NetMetrics {
        NetMetrics {
            served: self.served.load(Ordering::Relaxed),
            peers: self
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
}
