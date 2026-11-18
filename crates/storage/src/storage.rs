//! The `Storage` trait and its fjall implementation (ADR-0006).

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use crate::commit::CommitClock;
use crate::pool::StoragePool;
use crate::{Engine, Result, Timestamp, Txn, TxnOptions};

/// Entry point to the transactional storage layer.
pub trait Storage: Send + Sync + 'static {
    /// Begin a transaction reading at the current visible timestamp.
    fn begin(&self, options: TxnOptions) -> impl Future<Output = Result<Txn>> + Send;
}

/// Configuration for [`FjallStorage::open`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageOptions {
    /// Storage pool threads. Defaults to the number of cores, at least 2.
    pub pool_threads: usize,
    /// Bounded request queue length. Defaults to 1,024.
    pub queue_capacity: usize,
}

impl Default for StorageOptions {
    /// Return the ADR-0001 defaults.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, usize::from);
        Self {
            pool_threads: cores.max(2),
            queue_capacity: 1024,
        }
    }
}

/// State shared by the storage handle and every transaction.
pub(crate) struct Shared {
    /// Threads that run all fjall work.
    pub(crate) pool: StoragePool,
    /// Commit timestamps and the visibility watermark.
    pub(crate) clock: Arc<CommitClock>,
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
    /// [`Error::PoolUnavailable`](crate::Error::PoolUnavailable) if the pool
    /// threads can't be spawned.
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
        let engine = Engine::open(path)?;
        let clock = Arc::new(CommitClock::new(engine.last_commit_ts()?));
        let pool = StoragePool::start(&engine, options.pool_threads, options.queue_capacity)?;
        Ok(Self {
            shared: Arc::new(Shared { pool, clock }),
        })
    }

    /// Return the timestamp a transaction beginning now would read at.
    #[must_use]
    pub fn visible_ts(&self) -> Timestamp {
        self.shared.clock.visible_ts()
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
