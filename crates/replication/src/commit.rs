//! The Raft commit path (ADR-0037).
//!
//! A transaction commits through [`RaftCommit`], storage's
//! [`CommitPath`] for a replicated database:
//!
//! - **On the leader**, requests queue for the proposer task, which takes
//!   them in groups, validates each group with storage's [`Proposer`], and
//!   proposes the survivors as one log entry. The entry's outcome arrives
//!   once it commits and this node applies it.
//! - **On a follower**, the request is encoded and forwarded to the
//!   leader. Once the leader answers with the entry's index, the follower
//!   waits until it has applied that entry itself, so the transaction's
//!   next statement on this node sees its own commit.
//!
//! A request that surely didn't reach a leader, or that the leader refused
//! because it no longer leads, is retried until the commit timeout. One
//! that may have been proposed but whose outcome is lost, because the
//! leader changed or the connection failed, fails with
//! [`Error::CommitOutcomeUnknown`] and is never retried.
//!
//! A new leader proposes only after it has applied an entry of its own term,
//! the blank entry Raft commits at the start of every term. Every earlier
//! entry is then applied, so timestamps continue from the last commit and
//! validation sees every earlier write. If a proposal's outcome is lost,
//! the leader stops proposing for the rest of its term: later proposals of
//! that term might still commit, so their timestamps can't be reused.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use interlaken_storage::replica::{BarrierFuture, CommitFuture, CommitPath, Proposer, Replica};
use interlaken_storage::{CommitRequest, Error, Result, Timestamp};
use openraft::ServerState;
use openraft::async_runtime::WatchReceiver;
use openraft::error::ClientWriteError;
use openraft::impls::ProgressResponder;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep, timeout};

use crate::read::{Origin, Reader};
use crate::transport::{
    CommitError, Request, Response, Transport, TransportError, conflict_code, conflict_of,
};
use crate::types::{Batch, NodeId, Raft};

/// How the commit path groups and retries commits.
#[derive(Clone, Copy, Debug)]
pub struct CommitConfig {
    /// Most transactions proposed in one log entry.
    pub max_group: usize,
    /// How long a commit may wait for a leader, and for its entry to apply.
    pub timeout: Duration,
    /// How long to wait between attempts to reach a leader.
    pub retry: Duration,
    /// Whether a leader serves read indexes from its lease, without a round
    /// of heartbeats, while a quorum acknowledged it recently. The lease
    /// assumes clock drift between nodes is small against the election
    /// timeout.
    pub lease_reads: bool,
}

impl Default for CommitConfig {
    /// Return groups of up to 256, a five-second timeout, 20 ms retries
    /// and lease reads.
    fn default() -> Self {
        Self {
            max_group: 256,
            timeout: Duration::from_secs(5),
            retry: Duration::from_millis(20),
            lease_reads: true,
        }
    }
}

/// What a commit queued on this node came to.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Committed in the entry at this index, at this timestamp, and applied here.
    Committed(u64, Timestamp),
    /// This node doesn't lead; the request is handed back, untouched.
    NotLeader(CommitRequest),
    /// The commit failed, or its outcome is unknown.
    Failed(CommitError),
}

/// A commit queued for the proposer task.
struct Job {
    /// The transaction.
    request: CommitRequest,
    /// Where its outcome goes.
    reply: oneshot::Sender<Outcome>,
}

/// What the commit path needs once Raft is running.
struct Started<T> {
    /// The Raft handle.
    raft: Raft,
    /// The last log index applied here.
    applied: watch::Receiver<u64>,
    /// The connection to other replicas.
    transport: Arc<T>,
    /// Obtains read indexes for fresh snapshots.
    reader: Reader,
}

/// The Raft commit path: storage's [`CommitPath`] for a replica.
pub struct RaftCommit<T> {
    /// This node.
    id: NodeId,
    /// Grouping and retry settings.
    config: CommitConfig,
    /// The proposer task's queue.
    jobs: mpsc::Sender<Job>,
    /// The queue's other end, until the proposer task starts.
    pending: Mutex<Option<mpsc::Receiver<Job>>>,
    /// Raft and the transport, once started.
    started: OnceLock<Started<T>>,
}

impl<T> std::fmt::Debug for RaftCommit<T> {
    /// Render the commit path's node and settings.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RaftCommit")
            .field("id", &self.id)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Convert a failed commit's reason to storage's error.
fn to_error(error: CommitError) -> Error {
    match error {
        CommitError::NotLeader => {
            Error::ReplicationUnavailable("no leader accepted the commit".into())
        }
        CommitError::Unavailable(m) => Error::ReplicationUnavailable(m),
        CommitError::Conflict(code) => Error::SerializationFailure(conflict_of(code)),
        CommitError::TimestampExhausted => Error::TimestampExhausted,
        CommitError::Unknown(m) => {
            Error::CommitOutcomeUnknown(Arc::new(Error::ReplicationUnavailable(m)))
        }
    }
}

/// Convert storage's error from proposing to a commit's failure reason.
fn from_error(error: &Error) -> CommitError {
    match error {
        Error::SerializationFailure(conflict) => CommitError::Conflict(conflict_code(*conflict)),
        Error::TimestampExhausted => CommitError::TimestampExhausted,
        other => CommitError::Unavailable(other.to_string()),
    }
}

impl<T: Transport> RaftCommit<T> {
    /// Construct the commit path of node `id`; it refuses commits until started.
    #[must_use]
    pub fn new(id: NodeId, config: CommitConfig) -> Arc<Self> {
        let (jobs, pending) = mpsc::channel(config.max_group.max(1) * 4);
        Arc::new(Self {
            id,
            config,
            jobs,
            pending: Mutex::new(Some(pending)),
            started: OnceLock::new(),
        })
    }

    /// Start the proposer task, committing through `raft` to `replica`.
    ///
    /// # Panics
    ///
    /// Panics if called twice.
    pub fn start(
        &self,
        raft: Raft,
        replica: Replica,
        applied: watch::Receiver<u64>,
        transport: Arc<T>,
    ) {
        let jobs = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("the commit path starts once");
        let task = ProposerTask {
            id: self.id,
            config: self.config,
            raft: raft.clone(),
            replica,
            applied: applied.clone(),
            proposer: None,
            failed_term: Arc::new(AtomicU64::new(0)),
        };
        tokio::spawn(task.run(jobs));
        let reader = Reader::start(
            self.id,
            raft.clone(),
            Arc::clone(&transport),
            self.config.lease_reads,
            self.config.timeout,
        );
        let started = Started {
            raft,
            applied,
            transport,
            reader,
        };
        assert!(
            self.started.set(started).is_ok(),
            "the commit path starts once"
        );
    }

    /// Return the transport, once started.
    ///
    /// # Panics
    ///
    /// Panics before the commit path starts, which [`ReplicaNode::open`](crate::ReplicaNode::open) does.
    pub(crate) fn transport(&self) -> &T {
        &self
            .started
            .get()
            .expect("the commit path has started")
            .transport
    }

    /// Return a read index for another replica's fresh snapshot, if this node leads.
    pub(crate) async fn remote_read_index(&self) -> Result<u64, CommitError> {
        match self.started.get() {
            Some(started) => started.reader.read_index(Origin::Remote).await,
            None => Err(CommitError::NotLeader),
        }
    }

    /// Wait until this node has applied everything committed anywhere
    /// before the call, retrying until a leader answers or the timeout.
    async fn fresh(&self) -> Result<()> {
        let started = self
            .started
            .get()
            .ok_or_else(|| Error::ReplicationUnavailable("replication hasn't started".into()))?;
        let deadline = Instant::now() + self.config.timeout;
        loop {
            if let Ok(index) = started.reader.read_index(Origin::Local).await {
                let mut applied = started.applied.clone();
                let remaining = deadline.saturating_duration_since(Instant::now());
                return match timeout(remaining, applied.wait_for(|&i| i >= index)).await {
                    Ok(Ok(_)) => Ok(()),
                    _ => Err(Error::ReplicationUnavailable(format!(
                        "this replica didn't apply index {index} in time"
                    ))),
                };
            }
            if Instant::now() >= deadline {
                return Err(Error::ReplicationUnavailable(
                    "no leader confirmed a read index before the timeout".into(),
                ));
            }
            sleep(self.config.retry).await;
        }
    }

    /// Queue `request` for this node's proposer and return its outcome.
    pub(crate) async fn local(&self, request: CommitRequest) -> Outcome {
        let (reply, response) = oneshot::channel();
        if self.jobs.send(Job { request, reply }).await.is_err() {
            return Outcome::Failed(CommitError::Unavailable(
                "the commit path has stopped".into(),
            ));
        }
        response.await.unwrap_or_else(|_| {
            Outcome::Failed(CommitError::Unknown("the proposer stopped".into()))
        })
    }

    /// Wait until this node has applied the entry at `index`.
    async fn applied(&self, started: &Started<T>, index: u64) -> Result<()> {
        let mut applied = started.applied.clone();
        match timeout(self.config.timeout, applied.wait_for(|&i| i >= index)).await {
            Ok(Ok(_)) => Ok(()),
            _ => Err(Error::CommitOutcomeUnknown(Arc::new(
                Error::ReplicationUnavailable(format!(
                    "committed at index {index} but not applied here in time"
                )),
            ))),
        }
    }

    /// Commit `request` on the leader, here or by forwarding.
    async fn commit(&self, request: CommitRequest) -> Result<Timestamp> {
        let started = self
            .started
            .get()
            .ok_or_else(|| Error::ReplicationUnavailable("replication hasn't started".into()))?;
        let deadline = Instant::now() + self.config.timeout;
        let mut request = Some(request);
        let mut encoded: Option<bytes::Bytes> = None;
        loop {
            let leader = started.raft.metrics().borrow_watched().current_leader;
            match leader {
                Some(leader) if leader == self.id => {
                    let local = match request.take() {
                        Some(local) => local,
                        None => CommitRequest::decode(
                            encoded.as_ref().expect("encoded before forwarding"),
                        )?,
                    };
                    match self.local(local).await {
                        Outcome::Committed(_, ts) => return Ok(ts),
                        Outcome::NotLeader(back) => request = Some(back),
                        Outcome::Failed(error) => return Err(to_error(error)),
                    }
                }
                Some(leader) => {
                    let bytes = encoded.get_or_insert_with(|| {
                        request
                            .as_ref()
                            .expect("request kept until encoded")
                            .encode()
                    });
                    match started
                        .transport
                        .send(leader, Request::Commit(bytes.to_vec()))
                        .await
                    {
                        Ok(Response::Commit(Ok(committed))) => {
                            self.applied(started, committed.index).await?;
                            return Ok(Timestamp::from_raw(committed.ts));
                        }
                        Ok(
                            Response::Commit(Err(CommitError::NotLeader)) | Response::Refused(_),
                        )
                        | Err(TransportError::NotDelivered(_)) => {}
                        Ok(Response::Commit(Err(error))) => return Err(to_error(error)),
                        Ok(other) => {
                            return Err(to_error(CommitError::Unknown(format!(
                                "unexpected response {other:?}"
                            ))));
                        }
                        Err(TransportError::NoReply(m)) => {
                            return Err(to_error(CommitError::Unknown(m)));
                        }
                    }
                }
                None => {}
            }
            if Instant::now() >= deadline {
                return Err(Error::ReplicationUnavailable(
                    "no leader accepted the commit before the timeout".into(),
                ));
            }
            sleep(self.config.retry).await;
        }
    }
}

impl<T: Transport> CommitPath for RaftCommit<T> {
    /// Commit through the leader.
    fn submit(&self, request: CommitRequest) -> CommitFuture<'_> {
        Box::pin(self.commit(request))
    }

    /// Wait for a read index from the leader to apply here.
    fn read_barrier(&self) -> BarrierFuture<'_> {
        Box::pin(self.fresh())
    }
}

/// The leader's proposer loop.
struct ProposerTask {
    /// This node.
    id: NodeId,
    /// Grouping and retry settings.
    config: CommitConfig,
    /// The Raft handle.
    raft: Raft,
    /// The replica's storage.
    replica: Replica,
    /// The last log index applied here.
    applied: watch::Receiver<u64>,
    /// The proposer, with the term it proposes in.
    proposer: Option<(u64, Proposer)>,
    /// The latest term in which a proposal's outcome was lost.
    failed_term: Arc<AtomicU64>,
}

impl ProposerTask {
    /// Propose queued commits in groups until the queue closes.
    async fn run(mut self, mut jobs: mpsc::Receiver<Job>) {
        while let Some(first) = jobs.recv().await {
            let mut group = vec![first];
            while group.len() < self.config.max_group {
                match jobs.try_recv() {
                    Ok(job) => group.push(job),
                    Err(_) => break,
                }
            }
            self.propose(group).await;
        }
    }

    /// Return a proposer for the current term, if this node leads and may
    /// propose, waiting for the term's first entry to apply if it's new.
    async fn ready(&mut self) -> Option<&mut Proposer> {
        let metrics = self.raft.metrics().borrow_watched().clone();
        let term = metrics.current_term;
        if metrics.state != ServerState::Leader
            || metrics.current_leader != Some(self.id)
            || self.failed_term.load(Ordering::Acquire) >= term
        {
            self.proposer = None;
            return None;
        }
        if self.proposer.as_ref().map(|(t, _)| *t) != Some(term) {
            self.proposer = None;
            let applied_this_term = |m: &openraft::RaftMetrics<crate::types::TypeConfig>| {
                m.current_term != term
                    || m.last_applied
                        .as_ref()
                        .is_some_and(|id| id.committed_leader_id().term == term)
            };
            let metrics = self
                .raft
                .wait(Some(self.config.timeout))
                .metrics(applied_this_term, "the leader applies its first entry")
                .await
                .ok()?;
            if metrics.current_term != term {
                return None;
            }
            self.proposer = Some((term, self.replica.proposer()));
        }
        self.proposer.as_mut().map(|(_, p)| p)
    }

    /// Validate and propose one group, answering each job when its outcome is known.
    async fn propose(&mut self, group: Vec<Job>) {
        let term = self.raft.metrics().borrow_watched().current_term;
        let Some(proposer) = self.ready().await else {
            for job in group {
                let _ = job.reply.send(Outcome::NotLeader(job.request));
            }
            return;
        };
        let (requests, replies): (Vec<_>, Vec<_>) =
            group.into_iter().map(|j| (j.request, j.reply)).unzip();
        let (batch, outcomes) = proposer.propose(&requests);
        let Some(batch) = batch else {
            for (reply, outcome) in replies.into_iter().zip(outcomes) {
                let error = outcome.expect_err("no batch means every request failed");
                let _ = reply.send(Outcome::Failed(from_error(&error)));
            }
            return;
        };
        let (responder, written) = ProgressResponder::complete_only();
        if self
            .raft
            .client_write_ff(Batch(batch.to_vec()), Some(responder))
            .await
            .is_err()
        {
            self.failed_term.fetch_max(term, Ordering::AcqRel);
            self.proposer = None;
            for (reply, outcome) in replies.into_iter().zip(outcomes) {
                let failure = match outcome {
                    Ok(_) => CommitError::Unknown("raft stopped".into()),
                    Err(error) => from_error(&error),
                };
                let _ = reply.send(Outcome::Failed(failure));
            }
            return;
        }
        let failed_term = Arc::clone(&self.failed_term);
        let mut applied = self.applied.clone();
        let wait = self.config.timeout;
        tokio::spawn(async move {
            let result = match written.await {
                Ok(Ok(response)) => {
                    let index = response.log_id.index();
                    match timeout(wait, applied.wait_for(|&i| i >= index)).await {
                        Ok(Ok(_)) => Ok(index),
                        _ => Err(CommitError::Unknown(format!(
                            "committed at index {index} but not applied in time"
                        ))),
                    }
                }
                Ok(Err(ClientWriteError::ForwardToLeader(_))) => {
                    // Rejected before it was appended: it never commits.
                    failed_term.fetch_max(term, Ordering::AcqRel);
                    Err(CommitError::NotLeader)
                }
                Ok(Err(error)) => {
                    failed_term.fetch_max(term, Ordering::AcqRel);
                    Err(CommitError::Unknown(error.to_string()))
                }
                Err(_) => {
                    failed_term.fetch_max(term, Ordering::AcqRel);
                    Err(CommitError::Unknown("raft dropped the proposal".into()))
                }
            };
            for ((reply, request), outcome) in replies.into_iter().zip(requests).zip(outcomes) {
                let answer = match (&result, outcome) {
                    (_, Err(error)) => Outcome::Failed(from_error(&error)),
                    (Ok(index), Ok(ts)) => Outcome::Committed(*index, ts),
                    (Err(CommitError::NotLeader), Ok(_)) => Outcome::NotLeader(request),
                    (Err(error), Ok(_)) => Outcome::Failed(error.clone()),
                };
                let _ = reply.send(answer);
            }
        });
    }
}
