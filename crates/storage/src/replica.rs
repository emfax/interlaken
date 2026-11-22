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
//! The history must also outlive transactions running on other replicas,
//! whose snapshots this replica can't see pinned: it keeps every commit
//! applied within the history retention ([`Replica::set_history_retention`],
//! a minute by default), as well as those local pins need. A transaction
//! whose snapshot is older than that fails validation with `40001`.
//!
//! [`FjallStorage::open_replicated`]: crate::FjallStorage::open_replicated

use std::collections::VecDeque;
use std::future::Future;
use std::hash::{BuildHasher, RandomState};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::commit::{CommitClock, CommitHistory, CommitRequest, LogEntry, LogWrite};
use crate::engine::Engine;
use crate::pool::StoragePool;
use crate::{Error, Result, Timestamp};

/// The future [`CommitPath::submit`] returns.
pub type CommitFuture<'a> = Pin<Box<dyn Future<Output = Result<Timestamp>> + Send + 'a>>;

/// The future [`CommitPath::read_barrier`] returns.
pub type BarrierFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

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

    /// Wait until a transaction beginning on this node would see every
    /// commit acknowledged anywhere before the call: the barrier for
    /// fresh transactions (ADR-0037). A single node needs none.
    ///
    /// # Errors
    ///
    /// An error if the barrier can't be established, such as
    /// [`Error::ReplicationUnavailable`] without a leader.
    fn read_barrier(&self) -> BarrierFuture<'_> {
        Box::pin(std::future::ready(Ok(())))
    }
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
    /// The last timestamp of each applied entry within the retention, with
    /// when it was applied, oldest first.
    applied_at: VecDeque<(Timestamp, Instant)>,
    /// How long applied commits stay in the history for other replicas'
    /// transactions.
    retention: Duration,
}

/// How long applied commits stay in the history by default.
const DEFAULT_RETENTION: Duration = Duration::from_secs(60);

impl Validation {
    /// Construct the state for a database whose last commit is at `last`.
    pub(crate) fn new(last: Timestamp) -> Self {
        Self {
            history: CommitHistory::new(last),
            proposed: VecDeque::new(),
            // Random, so proposals from different replicas don't collide.
            next_id: RandomState::new().hash_one(last.into_raw()),
            applied_at: VecDeque::new(),
            retention: DEFAULT_RETENTION,
        }
    }

    /// Note that commits up to `last` were applied now, and trim the
    /// history to what local pins (`horizon`) and the retention still need.
    fn applied(&mut self, last: Timestamp, horizon: Timestamp) {
        let now = Instant::now();
        self.applied_at.push_back((last, now));
        // The newest commit applied before the retention began may go.
        let mut expired = None;
        while let Some(&(ts, at)) = self.applied_at.front()
            && now.duration_since(at) >= self.retention
        {
            expired = Some(ts);
            self.applied_at.pop_front();
        }
        if let Some(expired) = expired {
            self.history.trim(horizon.min(expired));
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

    /// Keep applied commits in the validation history for at least
    /// `retention`, so transactions on other replicas whose snapshots are
    /// that old can still be validated here once this replica leads.
    pub fn set_history_retention(&self, retention: Duration) {
        lock(&self.state.validation).retention = retention;
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
            validation.applied(entry.last_ts(), self.state.clock.horizon());
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

    /// Capture this replica's state as it is now, for a snapshot. Call it
    /// between applies; the view is unaffected by later ones.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn capture(&self) -> Result<SnapshotView> {
        let view = self.state.pool.run(Engine::view).await?;
        Ok(SnapshotView {
            pool: Arc::clone(&self.state.pool),
            view,
        })
    }

    /// Replace this replica's data with `snapshot`'s, made by
    /// [`SnapshotView::export`] on another replica, and continue from its
    /// last commit. Validation history restarts there, so transactions
    /// whose snapshots are older fail with `40001`.
    ///
    /// Store the snapshot durably first ([`Replica::store_snapshot`]): an
    /// import a crash interrupts is redone from it on restart
    /// ([`Replica::import_pending`]). Transactions reading while the data
    /// is replaced can see it incomplete.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptMeta`] for a malformed snapshot, or a storage error.
    pub async fn import(&self, snapshot: Vec<u8>) -> Result<()> {
        let last = self
            .state
            .pool
            .run(move |engine: &Engine| engine.import(&snapshot))
            .await??;
        let mut validation = lock(&self.state.validation);
        let retention = validation.retention;
        let next_id = validation.next_id;
        *validation = Validation::new(last);
        validation.retention = retention;
        validation.next_id = next_id;
        drop(validation);
        self.state.clock.publish(last);
        Ok(())
    }

    /// Return `true` if a crash interrupted a snapshot import, which must be
    /// redone from the stored snapshot before the replica is used.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn import_pending(&self) -> Result<bool> {
        self.state.pool.run(Engine::import_pending).await?
    }

    /// Replace the stored snapshot with `blob`, durably.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn store_snapshot(&self, blob: Vec<u8>) -> Result<()> {
        self.state
            .pool
            .run(move |engine: &Engine| engine.store_snapshot(&blob))
            .await?
    }

    /// Return the stored snapshot, if one was stored completely.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn stored_snapshot(&self) -> Result<Option<Vec<u8>>> {
        self.state.pool.run(Engine::stored_snapshot).await?
    }
}

/// A replica's state at one moment, from [`Replica::capture`].
pub struct SnapshotView {
    /// Runs the export off the async threads.
    pool: Arc<StoragePool>,
    /// fjall's consistent view.
    view: fjall::Snapshot,
}

impl std::fmt::Debug for SnapshotView {
    /// Render the view opaquely.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotView").finish_non_exhaustive()
    }
}

impl SnapshotView {
    /// Encode the captured data, last commit timestamp and GC watermark,
    /// with the `meta` entries at `meta_keys`, as one snapshot for
    /// [`Replica::import`].
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub async fn export(self, meta_keys: Vec<Vec<u8>>) -> Result<Vec<u8>> {
        let view = self.view;
        self.pool
            .run(move |engine: &Engine| engine.export(&view, &meta_keys))
            .await?
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
