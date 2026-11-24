//! Fresh snapshots: Raft's read index, batched (ADR-0037 phase 3).
//!
//! A fresh transaction must see every commit acknowledged anywhere before
//! it began. The leader knows the commit index; confirming that it still
//! leads, by a quorum's recent acknowledgement (its lease) or by a round of
//! heartbeats, makes that index a safe lower bound. The transaction's node
//! then waits until it has applied that index, and takes its snapshot.
//!
//! Requests are batched: the reader task takes every request queued so
//! far and answers them all with one read index. Each request was queued
//! before the read index was obtained, so the index covers every commit
//! acknowledged before any of them. A follower forwards one request per
//! batch to the leader, which batches what it receives with its own.

use std::sync::Arc;
use std::time::Duration;

use openraft::LinearizerOption;
use openraft::async_runtime::WatchReceiver;
use tokio::sync::{mpsc, oneshot};

use crate::telemetry;
use crate::transport::{CommitError, Request, Response, Transport};
use crate::types::{NodeId, Raft};

/// Where a read-index request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A transaction on this node: forwarded to the leader if need be.
    Local,
    /// Another replica: answered only if this node leads.
    Remote,
}

/// A queued read-index request.
struct ReadJob {
    /// Where it came from.
    origin: Origin,
    /// Where the index, or why there's none, goes.
    reply: oneshot::Sender<Result<u64, CommitError>>,
}

/// Obtains read indexes for this node, in batches.
#[derive(Debug, Clone)]
pub(crate) struct Reader {
    /// The reader task's queue.
    jobs: mpsc::UnboundedSender<ReadJob>,
}

impl Reader {
    /// Start the reader task of node `id`. With `lease`, a leader whose
    /// lease holds answers without a round trip.
    pub(crate) fn start<T: Transport>(
        id: NodeId,
        raft: Raft,
        transport: Arc<T>,
        lease: bool,
        wait: Duration,
    ) -> Self {
        let (jobs, queue) = mpsc::unbounded_channel();
        tokio::spawn(run(id, raft, transport, lease, wait, queue));
        Self { jobs }
    }

    /// Return a read index: the log index this node must apply before a
    /// fresh snapshot sees every commit acknowledged before the call.
    pub(crate) async fn read_index(&self, origin: Origin) -> Result<u64, CommitError> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(ReadJob { origin, reply })
            .map_err(|_| CommitError::Unavailable("the reader has stopped".into()))?;
        answer
            .await
            .unwrap_or_else(|_| Err(CommitError::Unavailable("the reader has stopped".into())))
    }
}

/// Answer queued requests in batches until the queue closes.
async fn run<T: Transport>(
    id: NodeId,
    raft: Raft,
    transport: Arc<T>,
    lease: bool,
    wait: Duration,
    mut queue: mpsc::UnboundedReceiver<ReadJob>,
) {
    let option =
        LinearizerOption::new((!lease).then_some(Duration::ZERO), true).with_wait_timeout(wait);
    while let Some(first) = queue.recv().await {
        let mut batch = vec![first];
        while let Ok(job) = queue.try_recv() {
            batch.push(job);
        }
        let leader = raft.metrics().borrow_watched().current_leader;
        if leader == Some(id) {
            let kind = if lease { "lease" } else { "read_index" };
            metrics::counter!(telemetry::READS, "kind" => kind).increment(batch.len() as u64);
            let index = match raft.get_read_log_id(option.clone()).await {
                Ok((read, _)) => Ok(read.index()),
                Err(_) => Err(CommitError::NotLeader),
            };
            for job in batch {
                let _ = job.reply.send(index.clone());
            }
            continue;
        }
        let (local, remote): (Vec<_>, Vec<_>) =
            batch.into_iter().partition(|j| j.origin == Origin::Local);
        for job in remote {
            let _ = job.reply.send(Err(CommitError::NotLeader));
        }
        if local.is_empty() {
            continue;
        }
        metrics::counter!(telemetry::READS, "kind" => "forwarded").increment(local.len() as u64);
        let index = match leader {
            Some(leader) => match transport.send(leader, Request::ReadIndex).await {
                Ok(Response::ReadIndex(index)) => index,
                // Reads are idempotent: whatever went wrong, ask again.
                _ => Err(CommitError::NotLeader),
            },
            None => Err(CommitError::NotLeader),
        };
        for job in local {
            let _ = job.reply.send(index.clone());
        }
    }
}
