//! What a replicated commit path needs from storage (ADR-0037).
//!
//! A [`FjallStorage`](crate::FjallStorage) commits through a [`CommitPath`].
//! Single-node databases use the built-in commit sequencer. A replicated
//! database is opened with [`FjallStorage::open_replicated`], which hands a
//! [`Replica`] to the caller's commit path, `interlaken-replication`'s Raft
//! commit path in practice. The replica offers the pieces that path needs:
//!
//! - **Proposing**, on the leader: a [`Proposer`] validates commit requests
//!   against the history of recent commits, as the sequencer does, gives
//!   the survivors consecutive timestamps, and returns them as one encoded
//!   batch to put in the log.
//! - **Applying**, on every replica: [`Replica::apply`] writes a committed
//!   batch's versions, with the caller's `meta` entries, in one batch,
//!   records the writes in the validation history, so any replica can
//!   become leader, and publishes the new visible timestamp.
//! - **The log and state** the Raft implementation keeps: a log keyspace
//!   of entries by index, and `meta` entries for its vote and progress.
//!
//! A new leader must have applied every entry before its term before it
//! asks for a proposer: timestamps continue from the last applied commit,
//! and validation needs every earlier commit in the history.
//!
//! A proposer records its proposals in the history as it makes them, so
//! later requests are validated against them. Each batch carries an
//! identifier, and applying an entry checks it against the oldest
//! outstanding proposal: if it matches, the history already holds it;
//! otherwise every outstanding proposal is a phantom that will never
//! commit, from a leadership this replica lost, and is forgotten before the
//! entry is recorded.
//!
//! [`FjallStorage::open_replicated`]: crate::FjallStorage::open_replicated

use std::collections::VecDeque;
use std::future::Future;
use std::hash::{BuildHasher, RandomState};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;

use crate::commit::{CommitClock, CommitHistory, CommitRequest, LogEntry, LogWrite};
use crate::engine::Engine;
use crate::pool::StoragePool;
use crate::{Error, Result, Timestamp};

/// The future [`CommitPath::submit`] returns.
pub type CommitFuture<'a> = Pin<Box<dyn Future<Output = Result<Timestamp>> + Send + 'a>>;

/// Where a transaction's commit goes: validation, ordering and durability.
///
/// The commit sequencer is the single-node implementation; a replicated
/// database supplies its own when opened with
/// [`FjallStorage::open_replicated`](crate::FjallStorage::open_replicated).
pub trait CommitPath: Send + Sync + 'static {
    /// Commit `request` and return its commit timestamp once a transaction
    /// beginning on this node sees it.
    ///
    /// # Errors
    ///
    /// [`Error::SerializationFailure`] if validation fails,
    /// [`Error::CommitOutcomeUnknown`] if the commit may or may not have
    /// happened, or another error if it surely didn't.
    fn submit(&self, request: CommitRequest) -> CommitFuture<'_>;
}

/// Lock `mutex`, ignoring poisoning: the guarded state stays consistent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The state a replica's proposer and apply step share.
pub(crate) struct ReplicaState {
    /// Runs fjall calls off the async threads (ADR-0001).
    pub(crate) pool: Arc<StoragePool>,
    /// The visibility watermark and pinned snapshots.
    pub(crate) clock: Arc<CommitClock>,
    /// Recent commits, for validation, and outstanding proposals.
    pub(crate) validation: Mutex<Validation>,
}

/// The validation history with the proposals recorded in it ahead of the log.
pub(crate) struct Validation {
    /// Recent commits, including outstanding proposals.
    pub(crate) history: CommitHistory,
    /// Identifiers of proposals recorded but not yet applied, oldest first.
    pub(crate) proposed: VecDeque<u64>,
    /// The identifier of the next proposal.
    pub(crate) next_id: u64,
}

impl Validation {
    /// Construct the state for a database whose last commit is at `last`.
    pub(crate) fn new(last: Timestamp) -> Self {
        Self {
            history: CommitHistory::new(last),
            proposed: VecDeque::new(),
            // Random, so proposals from different replicas don't collide.
            next_id: RandomState::new().hash_one(last.into_raw()),
        }
    }

    /// Forget every outstanding proposal: they are recorded after `applied`.
    fn forget_proposals(&mut self, applied: Timestamp) {
        self.history.truncate_after(applied);
        self.proposed.clear();
    }
}

/// Storage's side of a replica: proposing, applying, and the log and state
/// the Raft implementation keeps. Cloning is cheap.
#[derive(Clone)]
pub struct Replica {
    /// The shared state.
    state: Arc<ReplicaState>,
}

impl std::fmt::Debug for Replica {
    /// Render the replica with its visible timestamp.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replica")
            .field("visible_ts", &self.visible_ts())
            .finish_non_exhaustive()
    }
}

impl Replica {
    /// Wrap the shared state.
    pub(crate) fn new(state: Arc<ReplicaState>) -> Self {
        Self { state }
    }

    /// Return the timestamp of the last commit applied here.
    #[must_use]
    pub fn visible_ts(&self) -> Timestamp {
        self.state.clock.visible_ts()
    }

    /// Return a proposer that continues after the last applied commit,
    /// forgetting any proposals this replica recorded that never committed.
    ///
    /// Call it only once every log entry before the leader's term is applied.
    #[must_use]
    pub fn proposer(&self) -> Proposer {
        let applied = self.visible_ts();
        lock(&self.state.validation).forget_proposals(applied);
        Proposer {
            state: Arc::clone(&self.state),
            next: applied,
        }
    }

    /// Apply a committed entry: write `batch`'s versions, if it has one,
    /// with the `meta` entries given, in one unsynced batch, record them for
    /// validation and publish them. The log is the durable record.
    ///
    /// Entries must be applied in log order.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptMeta`] for a malformed batch, or a storage error.
    pub async fn apply(&self, batch: Option<Bytes>, meta: Vec<(Vec<u8>, Vec<u8>)>) -> Result<()> {
        let entry = batch.map(|b| LogEntry::decode(&b)).transpose()?;
        let written = entry.clone();
        self.state
            .pool
            .run(move |engine: &Engine| match &written {
                Some(entry) => engine.apply_versions(
                    entry
                        .versions()
                        .map(|(ts, w)| (ts, &w.key[..], w.value.as_deref())),
                    Some(entry.last_ts()),
                    &meta,
                ),
                None => engine.apply_versions(std::iter::empty(), None, &meta),
            })
            .await??;
        if let Some(entry) = entry {
            let mut validation = lock(&self.state.validation);
            if validation.proposed.front() == Some(&entry.index) {
                // This replica proposed the entry and recorded it then.
                validation.proposed.pop_front();
            } else {
                validation.forget_proposals(self.visible_ts());
                for (writes, ts) in entry.txns.iter().zip(entry.first_ts.into_raw()..) {
                    let keys = writes.iter().map(|w| w.key.clone()).collect();
                    validation.history.record(Timestamp::from_raw(ts), keys);
                }
            }
            validation.history.trim(self.state.clock.horizon());
            drop(validation);
            self.state.clock.publish(entry.last_ts());
        }
        Ok(())
    }

    /// Return the `meta` entry at `key`, if any.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn meta(&self, key: Vec<u8>) -> Result<Option<Vec<u8>>> {
        self.state
            .pool
            .run(move |engine: &Engine| engine.get_meta(&key))
            .await?
    }

    /// Set `meta` entries in one batch, synced if `sync`.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn set_meta(&self, entries: Vec<(Vec<u8>, Vec<u8>)>, sync: bool) -> Result<()> {
        self.state
            .pool
            .run(move |engine: &Engine| engine.set_meta(&entries, sync))
            .await?
    }

    /// Append log entries in one batch, synced if `sync`, else written to
    /// OS buffers.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn log_append(&self, entries: Vec<(u64, Vec<u8>)>, sync: bool) -> Result<()> {
        self.state
            .pool
            .run(move |engine: &Engine| engine.replica_log_append(&entries, sync))
            .await?
    }

    /// Return the log entries with indexes in `start..end`, in order.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn log_range(&self, start: u64, end: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        self.state
            .pool
            .run(move |engine: &Engine| engine.replica_log_range(start, end))
            .await?
    }

    /// Return the first and last log entries, if any.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn log_bounds(&self) -> Result<Option<((u64, Vec<u8>), (u64, Vec<u8>))>> {
        self.state
            .pool
            .run(|engine: &Engine| -> Result<_> {
                Ok(engine.replica_log_first()?.zip(engine.replica_log_last()?))
            })
            .await?
    }

    /// Remove the log entries with indexes in `start..end`, synced if `sync`.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn log_remove(&self, start: u64, end: u64, sync: bool) -> Result<()> {
        self.state
            .pool
            .run(move |engine: &Engine| engine.replica_log_remove(start, end, sync))
            .await?
    }

    /// Sync everything written so far.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn persist(&self) -> Result<()> {
        self.state.pool.run(Engine::persist).await?
    }
}

/// Validates commits on the leader and turns each group into one batch
/// for the log (see the module documentation).
pub struct Proposer {
    /// The shared state.
    state: Arc<ReplicaState>,
    /// The last timestamp given out.
    next: Timestamp,
}

impl std::fmt::Debug for Proposer {
    /// Render the proposer with its last timestamp.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proposer")
            .field("next", &self.next)
            .finish_non_exhaustive()
    }
}

impl Proposer {
    /// Validate `requests` in order, against recent commits and each other,
    /// and return the batch of the survivors, if any, with each request's
    /// outcome: its commit timestamp once the batch commits, or why it can't.
    ///
    /// The survivors are recorded for validating later requests, as the
    /// batch comes first in the log.
    pub fn propose(
        &mut self,
        requests: &[CommitRequest],
    ) -> (Option<Bytes>, Vec<Result<Timestamp>>) {
        let mut validation = lock(&self.state.validation);
        let validation = &mut *validation;
        let history = &mut validation.history;
        let mut txns = Vec::new();
        let mut first = None;
        let outcomes = requests
            .iter()
            .map(|request| {
                history
                    .validate(request)
                    .map_err(Error::SerializationFailure)?;
                let ts = self.next.successor().ok_or(Error::TimestampExhausted)?;
                self.next = ts;
                first.get_or_insert(ts);
                history.record(ts, request.writes.iter().map(|w| w.key.clone()).collect());
                txns.push(
                    request
                        .writes
                        .iter()
                        .map(|w| LogWrite {
                            key: w.key.clone(),
                            value: w.op.value().cloned(),
                        })
                        .collect(),
                );
                Ok(ts)
            })
            .collect();
        let batch = first.map(|first_ts| {
            let id = validation.next_id;
            validation.next_id = id.wrapping_add(1);
            validation.proposed.push_back(id);
            // A replicated batch's index field carries its proposal's identifier.
            LogEntry {
                index: id,
                first_ts,
                txns,
            }
            .encode()
        });
        (batch, outcomes)
    }
}
