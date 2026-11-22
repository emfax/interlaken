//! The `Storage` trait and its fjall implementation (ADR-0006).

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::commit::{CommitClock, CommitStats, Sequencer, SequencerConfig, SequencerStats};
use crate::engine::{DEFAULT_MAX_JOURNAL_BYTES, DEFAULT_MEMTABLE_BYTES};
use crate::gc::{GcContext, GcCounters, GcReport, GcStats, GcTask};
use crate::pool::StoragePool;
use crate::replica::{CommitPath, Replica, ReplicaState, Validation};
use crate::{Engine, Result, Timestamp, Txn, TxnOptions};

/// Entry point to the transactional storage layer.
pub trait Storage: Send + Sync + 'static {
    /// Begin a transaction reading at the current visible timestamp.
    fn begin(&self, options: TxnOptions) -> impl Future<Output = Result<Txn>> + Send;
}

/// When a commit is acknowledged (ADR-0005).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Durability {
    /// Sync the journal (`persist(SyncAll)`) once per commit group before
    /// acknowledging. Acknowledged commits survive power loss.
    #[default]
    Sync,
    /// Acknowledge once the group reaches OS buffers. Commits survive a
    /// process crash but not power loss. For development and tests.
    Buffered,
}

/// Configuration for [`FjallStorage::open`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageOptions {
    /// Storage pool threads. Defaults to the number of cores, at least 2.
    pub pool_threads: usize,
    /// Bounded storage pool queue length. Defaults to 1,024.
    pub queue_capacity: usize,
    /// When commits are acknowledged. Defaults to [`Durability::Sync`].
    pub durability: Durability,
    /// Most transactions the sequencer writes in one group. Defaults to 256.
    pub max_commit_group: usize,
    /// Bounded commit queue length. Defaults to 1,024.
    pub commit_queue_capacity: usize,
    /// Most bytes of keys plus values one transaction may buffer. Defaults to 64 MiB.
    pub max_write_set_bytes: usize,
    /// Longest a transaction may stay open. Defaults to 5 minutes.
    ///
    /// An older transaction fails its next operation with
    /// [`Error::TransactionTooOld`](crate::Error::TransactionTooOld) and no
    /// longer holds back version GC (ADR-0004, ADR-0008). `Duration::MAX`
    /// disables the limit.
    pub max_transaction_age: Duration,
    /// How often the background GC pass runs, or `None` to run GC only
    /// through [`FjallStorage::run_gc`]. Defaults to 10 seconds.
    pub gc_interval: Option<Duration>,
    /// Versions one GC chunk examines before yielding the storage thread.
    /// Defaults to 4,096.
    pub gc_chunk_versions: usize,
    /// Most bytes of journal fjall keeps before flushing memtables so the
    /// oldest journal can be deleted. Defaults to 512 MiB; values below
    /// 64 MiB are raised to 64 MiB.
    ///
    /// Under sustained writes the journals dominate the on-disk footprint,
    /// which saws between roughly the live data and this cap plus the
    /// memtables (ADR-0008, *As implemented (F4)*).
    pub max_journal_bytes: u64,
    /// Size at which a keyspace's memtable is flushed to a table. Defaults
    /// to 64 MiB; values below 1 MiB are raised to 1 MiB. Recorded when the
    /// database is created; later opens keep the original size.
    pub memtable_bytes: u64,
    /// Commit through the commit log (ADR-0037 phase 1): each validated
    /// group is first appended to the `raftlog` keyspace, which is the
    /// commit, and then applied to the data by a separate thread, as
    /// replication will need. `false` is the single-node path. Defaults to
    /// `false`, or `true` when `INTERLAKEN_COMMIT_LOG` is `1`; the
    /// environment variable lets every test suite run on either path unchanged.
    ///
    /// A database can switch between the two paths from one open to the
    /// next: opening it applies anything left in the log first.
    pub commit_log: bool,
}

impl Default for StorageOptions {
    /// Return the defaults from ADR-0001, ADR-0004, ADR-0005 and ADR-0008.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, usize::from);
        Self {
            pool_threads: cores.max(2),
            queue_capacity: 1024,
            durability: Durability::Sync,
            max_commit_group: 256,
            commit_queue_capacity: 1024,
            max_write_set_bytes: 64 * 1024 * 1024,
            max_transaction_age: Duration::from_secs(5 * 60),
            gc_interval: Some(Duration::from_secs(10)),
            gc_chunk_versions: 4096,
            max_journal_bytes: DEFAULT_MAX_JOURNAL_BYTES,
            memtable_bytes: DEFAULT_MEMTABLE_BYTES,
            commit_log: std::env::var_os("INTERLAKEN_COMMIT_LOG").is_some_and(|v| v == "1"),
        }
    }
}

/// Per-transaction limits (ADR-0004).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) max_write_set_bytes: usize,
    pub(crate) max_transaction_age: Duration,
}

/// A point-in-time view of the storage layer's state and counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StorageMetrics {
    /// The timestamp new snapshots read at.
    pub visible_ts: Timestamp,
    /// Open transactions within the maximum age.
    pub active_transactions: usize,
    /// The oldest snapshot an open transaction holds, if any.
    pub oldest_snapshot: Option<Timestamp>,
    /// Transactions that exceeded the maximum age since the database was opened.
    pub expired_transactions: u64,
    /// Commit pipeline counters.
    pub commit: CommitStats,
    /// Version GC counters.
    pub gc: GcStats,
    /// Space the database uses on disk, in bytes.
    pub disk_space_bytes: u64,
}

/// State shared by the storage handle and every transaction.
///
/// Fields drop in order: the GC thread stops before the pool it uses.
pub(crate) struct Shared {
    /// The background GC thread, if enabled.
    pub(crate) _gc_task: Option<GcTask>,
    /// What a GC pass needs.
    pub(crate) gc: GcContext,
    /// Threads that run all fjall reads and GC chunks.
    pub(crate) pool: Arc<StoragePool>,
    /// The visibility watermark and pinned snapshots.
    pub(crate) clock: Arc<CommitClock>,
    /// Where commits go: the sequencer, or a replicated commit path.
    pub(crate) commit: Arc<dyn crate::replica::CommitPath>,
    /// Counters the sequencer updates.
    pub(crate) stats: Arc<SequencerStats>,
    /// Per-transaction limits.
    pub(crate) limits: Limits,
}

/// [`Storage`] backed by fjall, with all fjall calls on a dedicated pool.
///
/// Cloning is cheap; clones share the pool and database.
#[derive(Clone)]
pub struct FjallStorage {
    shared: Arc<Shared>,
}

impl FjallStorage {
    /// Open or create a database in the directory at `path` and start the storage pool.
    ///
    /// This calls fjall synchronously, so call it during startup outside an
    /// async task, or through `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Returns any error from [`Engine::open`], or
    /// [`Error::PoolUnavailable`](crate::Error::PoolUnavailable) if the pool,
    /// sequencer or GC threads can't be spawned.
    ///
    /// # Examples
    ///
    /// ```
    /// use bytes::Bytes;
    /// use interlaken_storage::{FjallStorage, Storage, StorageOptions, TxnOptions};
    ///
    /// # tokio::runtime::Runtime::new()?.block_on(async {
    /// let dir = tempfile::tempdir()?;
    /// let storage = FjallStorage::open(dir.path(), StorageOptions::default())?;
    ///
    /// let txn = storage.begin(TxnOptions::default()).await?;
    /// txn.put(Bytes::from_static(b"k"), Bytes::from_static(b"v"))?;
    /// txn.commit().await?;
    ///
    /// let txn = storage.begin(TxnOptions::default().read_only()).await?;
    /// let values = txn.get_many(vec![Bytes::from_static(b"k")]).await?;
    /// assert_eq!(values, vec![Some(Bytes::from_static(b"v"))]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn open(path: impl AsRef<Path>, options: StorageOptions) -> Result<Self> {
        Self::open_with(
            path,
            &options,
            |engine, clock, _pool, last, last_index, stats| {
                let config = SequencerConfig {
                    durability: options.durability,
                    max_group: options.max_commit_group,
                    queue_capacity: options.commit_queue_capacity,
                    log: options.commit_log,
                };
                let sequencer = Sequencer::start(engine, clock, last, last_index, config, stats)?;
                Ok(Arc::new(sequencer))
            },
        )
    }

    /// Open or create a replicated database: commits go through the
    /// [`CommitPath`] that `commit_path` builds from the database's
    /// [`Replica`], instead of the commit sequencer (ADR-0037).
    ///
    /// `options.commit_log`, `max_commit_group` and `commit_queue_capacity`
    /// don't apply; the commit path decides how commits are grouped.
    ///
    /// # Errors
    ///
    /// As [`FjallStorage::open`], or `commit_path`'s error.
    pub fn open_replicated(
        path: impl AsRef<Path>,
        options: &StorageOptions,
        commit_path: impl FnOnce(Replica) -> Result<Arc<dyn CommitPath>>,
    ) -> Result<Self> {
        Self::open_with(
            path,
            options,
            |_engine, clock, pool, last, _index, _stats| {
                let state = Arc::new(ReplicaState {
                    pool,
                    clock,
                    validation: std::sync::Mutex::new(Validation::new(last)),
                });
                commit_path(Replica::new(state))
            },
        )
    }

    /// Open the database and its threads, committing through the path
    /// `commit_path` builds from the engine, clock, pool, last commit
    /// timestamp, last commit log index and sequencer counters.
    fn open_with(
        path: impl AsRef<Path>,
        options: &StorageOptions,
        commit_path: impl FnOnce(
            Engine,
            Arc<CommitClock>,
            Arc<StoragePool>,
            Timestamp,
            u64,
            Arc<SequencerStats>,
        ) -> Result<Arc<dyn CommitPath>>,
    ) -> Result<Self> {
        let engine = Engine::open_with(path, options.max_journal_bytes, options.memtable_bytes)?;
        // Entries committed to the log but not applied before the last
        // close or crash are applied first, whichever path follows.
        let (last, last_index) = engine.replay_log()?;
        let clock = Arc::new(CommitClock::new(last));
        let pool = Arc::new(StoragePool::start(
            &engine,
            options.pool_threads,
            options.queue_capacity,
        )?);
        let stats = Arc::new(SequencerStats::default());
        let commit = commit_path(
            engine,
            Arc::clone(&clock),
            Arc::clone(&pool),
            last,
            last_index,
            Arc::clone(&stats),
        )?;
        let gc = GcContext {
            pool: Arc::clone(&pool),
            clock: Arc::clone(&clock),
            counters: Arc::new(GcCounters::default()),
            chunk_versions: options.gc_chunk_versions.max(1),
        };
        let gc_task = options
            .gc_interval
            .map(|interval| GcTask::start(gc.clone(), interval))
            .transpose()?;
        Ok(Self {
            shared: Arc::new(Shared {
                _gc_task: gc_task,
                gc,
                pool,
                clock,
                commit,
                stats,
                limits: Limits {
                    max_write_set_bytes: options.max_write_set_bytes,
                    max_transaction_age: options.max_transaction_age,
                },
            }),
        })
    }

    /// Return the timestamp a transaction beginning now would read at.
    #[must_use]
    pub fn visible_ts(&self) -> Timestamp {
        self.shared.clock.visible_ts()
    }

    /// Return the commit pipeline counters accumulated since the database was opened.
    #[must_use]
    pub fn commit_stats(&self) -> CommitStats {
        self.shared.stats.snapshot()
    }

    /// Run one version GC pass now and wait for it (ADR-0008).
    ///
    /// The background task runs passes on its own schedule; this is for
    /// tests, tools and operators. Transactions past the maximum age stop
    /// holding back the watermark.
    ///
    /// # Errors
    ///
    /// Returns a storage error if a chunk or the watermark write fails.
    pub async fn run_gc(&self) -> Result<GcReport> {
        self.shared.gc.pass().await
    }

    /// Return a snapshot of the storage layer's state and counters.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the disk usage can't be read.
    pub async fn metrics(&self) -> Result<StorageMetrics> {
        let disk_space_bytes = self.shared.pool.run(Engine::disk_space).await??;
        let (active_transactions, oldest_snapshot) = self.shared.clock.active();
        Ok(StorageMetrics {
            visible_ts: self.visible_ts(),
            active_transactions,
            oldest_snapshot,
            expired_transactions: self.shared.clock.expired_count(),
            commit: self.commit_stats(),
            gc: self.shared.gc.counters.snapshot(),
            disk_space_bytes,
        })
    }
}

impl Storage for FjallStorage {
    /// Begin a transaction at the current visible timestamp; no storage call is needed.
    async fn begin(&self, options: TxnOptions) -> Result<Txn> {
        Ok(Txn::begin(self.shared.clone(), options))
    }
}

impl std::fmt::Debug for FjallStorage {
    /// Render the handle with its visible timestamp.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FjallStorage")
            .field("visible_ts", &self.visible_ts())
            .finish_non_exhaustive()
    }
}
