//! The cloneable transaction handle (ADR-0006).

use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;

use super::read_set::ReadSet;
use super::write_buffer::{WriteBuffer, WriteOp};
use super::{Isolation, TxnOptions};
use crate::encoding::{EncodeError, MAX_TUPLE_LEN};
use crate::engine::mvcc::{self, ScanJob};
use crate::scan::{Key, KeyRange, ScanBatch, ScanRequest};
use crate::storage::Shared;
use crate::{Error, Result, Timestamp, commit};

/// Outcome of a successful commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    /// The commit timestamp, or `None` if the transaction wrote nothing.
    pub commit_ts: Option<Timestamp>,
}

/// A transaction.
///
/// A cheap cloneable handle: the operators of one query (join leaves, index
/// lookups, DML sinks) each hold a clone and use the transaction at the same
/// time. The snapshot, statement counter, write buffer and read set sit
/// behind one internal lock, which is never held across an `.await`.
///
/// Writes are buffered here and touch storage only at commit. Reads see the
/// snapshot plus this transaction's writes from earlier statements; a
/// statement never sees its own writes (ADR-0004).
#[derive(Clone)]
pub struct Txn(Arc<TxnInner>);

/// State behind a [`Txn`] handle.
struct TxnInner {
    shared: Arc<Shared>,
    options: TxnOptions,
    state: Mutex<TxnState>,
}

/// Mutable transaction state.
struct TxnState {
    snapshot: Timestamp,
    statement: u64,
    writes: WriteBuffer,
    reads: ReadSet,
}

impl Txn {
    /// Construct a transaction reading at the current visible timestamp.
    pub(crate) fn begin(shared: Arc<Shared>, options: TxnOptions) -> Self {
        let snapshot = shared.clock.visible_ts();
        Self(Arc::new(TxnInner {
            shared,
            options,
            state: Mutex::new(TxnState {
                snapshot,
                statement: 0,
                writes: WriteBuffer::default(),
                reads: ReadSet::default(),
            }),
        }))
    }

    /// Return the options the transaction was started with.
    #[must_use]
    pub fn options(&self) -> TxnOptions {
        self.0.options
    }

    /// Return the snapshot timestamp reads currently use.
    #[must_use]
    pub fn snapshot(&self) -> Timestamp {
        self.state().snapshot
    }

    /// Read several keys in one storage call.
    ///
    /// Returns one entry per key, in order: `None` if the key doesn't exist at
    /// the snapshot or was deleted by an earlier statement. Every key enters
    /// the read set.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage pool is unavailable or fjall fails.
    pub async fn get_many(&self, keys: Vec<Key>) -> Result<Vec<Option<Bytes>>> {
        let (snapshot, mut results, missing) = {
            let mut state = self.state();
            let statement = state.statement;
            let results: Vec<Option<Option<Bytes>>> = keys
                .iter()
                .map(|key| {
                    state
                        .writes
                        .visible(key, statement)
                        .map(|op| op.value().cloned())
                })
                .collect();
            for key in &keys {
                state.reads.record_key(key.clone());
            }
            let missing: Vec<(usize, Key)> = results
                .iter()
                .enumerate()
                .filter(|(_, r)| r.is_none())
                .map(|(i, _)| (i, keys[i].clone()))
                .collect();
            (state.snapshot, results, missing)
        };

        if !missing.is_empty() {
            let (indices, missing_keys): (Vec<usize>, Vec<Key>) = missing.into_iter().unzip();
            let fetched = self
                .0
                .shared
                .pool
                .run(move |engine| mvcc::read_many(engine.data(), &missing_keys, snapshot))
                .await??;
            for (i, value) in indices.into_iter().zip(fetched) {
                results[i] = Some(value);
            }
        }
        Ok(results.into_iter().map(Option::flatten).collect())
    }

    /// Read one batch of a range scan.
    ///
    /// Continue a scan by passing the batch's resume key in the next request
    /// (see [`ScanRequest::resume_after`]). The range each call actually
    /// examined enters the read set, including rows the filter rejected.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage pool is unavailable or fjall fails.
    pub async fn scan(&self, request: ScanRequest) -> Result<ScanBatch> {
        let range = match (request.resume, request.reverse) {
            (None, _) => request.range,
            (Some(resume), false) => KeyRange::new(Bound::Excluded(resume), request.range.end),
            (Some(resume), true) => KeyRange::new(request.range.start, Bound::Excluded(resume)),
        };
        if range.is_empty() {
            return Ok(ScanBatch {
                rows: Vec::new(),
                resume: None,
            });
        }
        let job = {
            let state = self.state();
            ScanJob {
                buffered: state.writes.visible_range(&range, state.statement),
                range,
                reverse: request.reverse,
                limit_rows: request.limit_rows,
                limit_bytes: request.limit_bytes,
                filter: request.filter,
                snapshot: state.snapshot,
            }
        };
        let output = self
            .0
            .shared
            .pool
            .run(move |engine| mvcc::scan(engine.data(), job))
            .await??;
        self.state().reads.record_range(output.covered);
        Ok(ScanBatch {
            rows: output.rows,
            resume: output.resume,
        })
    }

    /// Buffer a write setting `key` to `value`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReadOnlyTransaction`] in a read-only transaction, or
    /// [`Error::Encode`] if the key is too long to store.
    pub fn put(&self, key: Key, value: Bytes) -> Result<()> {
        self.write(key, WriteOp::Put(value))
    }

    /// Buffer a write deleting `key`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReadOnlyTransaction`] in a read-only transaction, or
    /// [`Error::Encode`] if the key is too long to store.
    pub fn delete(&self, key: Key) -> Result<()> {
        self.write(key, WriteOp::Delete)
    }

    /// Register a key read outside [`get_many`](Self::get_many) and [`scan`](Self::scan).
    pub fn register_read(&self, key: Key) {
        self.state().reads.record_key(key);
    }

    /// Register a range read outside [`get_many`](Self::get_many) and [`scan`](Self::scan).
    pub fn register_range(&self, range: KeyRange) {
        self.state().reads.record_range(range);
    }

    /// Mark a statement boundary.
    ///
    /// Writes from the statement just finished become visible to reads.
    /// Under [`Isolation::ReadCommitted`] the snapshot also refreshes to the
    /// current visible timestamp.
    pub fn next_statement(&self) {
        let fresh = self.0.shared.clock.visible_ts();
        let mut state = self.state();
        state.statement += 1;
        if self.0.options.isolation == Isolation::ReadCommitted {
            state.snapshot = fresh;
        }
    }

    /// Commit the transaction.
    ///
    /// # Errors
    ///
    /// - [`Error::TransactionShared`] if other clones of this handle are still
    ///   alive. This handle is consumed; a remaining clone can commit later.
    /// - [`Error::TimestampExhausted`], [`Error::Fjall`] or
    ///   [`Error::PoolUnavailable`] if the commit can't be written.
    pub async fn commit(self) -> Result<CommitInfo> {
        let inner = Arc::try_unwrap(self.0).map_err(|_| Error::TransactionShared)?;
        let state = inner
            .state
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        if state.writes.is_empty() {
            return Ok(CommitInfo { commit_ts: None });
        }
        let writes = state.writes.into_final_writes();
        let clock = inner.shared.clock.clone();
        let ts = inner
            .shared
            .pool
            .run(move |engine| commit::apply(engine, &clock, &writes))
            .await??;
        Ok(CommitInfo {
            commit_ts: Some(ts),
        })
    }

    /// Abort the transaction, discarding its writes.
    pub fn abort(self) {
        drop(self);
    }

    /// Buffer a write after checking it is allowed.
    fn write(&self, key: Key, op: WriteOp) -> Result<()> {
        if self.0.options.read_only {
            return Err(Error::ReadOnlyTransaction);
        }
        if key.len() > MAX_TUPLE_LEN {
            return Err(EncodeError::KeyTooLong {
                len: key.len(),
                max: MAX_TUPLE_LEN,
            }
            .into());
        }
        let mut state = self.state();
        let statement = state.statement;
        state.writes.write(key, op, statement);
        Ok(())
    }

    /// Lock the transaction state.
    ///
    /// Poisoning is ignored: every critical section leaves the state consistent.
    fn state(&self) -> MutexGuard<'_, TxnState> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Return a copy of the read set, for tests.
    #[cfg(test)]
    pub(crate) fn read_set(&self) -> ReadSet {
        self.state().reads.clone()
    }
}

impl std::fmt::Debug for Txn {
    /// Render the transaction's options and snapshot.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Txn")
            .field("options", &self.0.options)
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FjallStorage, Storage, StorageOptions};

    /// Open storage in a fresh temporary directory with a small pool.
    fn open() -> (tempfile::TempDir, FjallStorage) {
        let dir = tempfile::tempdir().unwrap();
        let options = StorageOptions {
            pool_threads: 2,
            queue_capacity: 16,
        };
        let storage = FjallStorage::open(dir.path(), options).unwrap();
        (dir, storage)
    }

    /// Construct a key from a static string.
    fn k(s: &'static str) -> Key {
        Bytes::from_static(s.as_bytes())
    }

    /// Commit `pairs` in one transaction.
    async fn seed(storage: &FjallStorage, pairs: &[(&'static str, &'static str)]) {
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        for (key, value) in pairs {
            txn.put(k(key), k(value)).unwrap();
        }
        txn.commit().await.unwrap();
    }

    #[tokio::test]
    /// Assert reads see earlier statements' writes but not the current statement's.
    async fn statement_visibility() {
        let (_dir, storage) = open();
        seed(&storage, &[("a", "old")]).await;
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        txn.put(k("a"), k("new")).unwrap();
        txn.put(k("b"), k("new")).unwrap();
        assert_eq!(
            txn.get_many(vec![k("a"), k("b")]).await.unwrap(),
            vec![Some(k("old")), None]
        );
        let rows = txn
            .scan(ScanRequest::new(KeyRange::all()))
            .await
            .unwrap()
            .rows;
        assert_eq!(rows, vec![(k("a"), k("old"))]);

        txn.next_statement();
        txn.delete(k("a")).unwrap();
        assert_eq!(
            txn.get_many(vec![k("a"), k("b")]).await.unwrap(),
            vec![Some(k("new")), Some(k("new"))]
        );
        txn.next_statement();
        let rows = txn
            .scan(ScanRequest::new(KeyRange::all()))
            .await
            .unwrap()
            .rows;
        assert_eq!(rows, vec![(k("b"), k("new"))]);
    }

    #[tokio::test]
    /// Assert snapshots are stable except across READ COMMITTED statement boundaries.
    async fn snapshot_refresh_only_for_read_committed() {
        let (_dir, storage) = open();
        seed(&storage, &[("a", "1")]).await;
        let rr = storage
            .begin(TxnOptions::new(Isolation::RepeatableRead))
            .await
            .unwrap();
        let rc = storage
            .begin(TxnOptions::new(Isolation::ReadCommitted))
            .await
            .unwrap();
        seed(&storage, &[("a", "2")]).await;

        rr.next_statement();
        rc.next_statement();
        assert_eq!(rr.get_many(vec![k("a")]).await.unwrap(), vec![Some(k("1"))]);
        assert_eq!(rc.get_many(vec![k("a")]).await.unwrap(), vec![Some(k("2"))]);
    }

    #[tokio::test]
    /// Assert a scan records the range it covered, up to its resume key.
    async fn scan_records_covered_range() {
        let (_dir, storage) = open();
        seed(&storage, &[("a", "1"), ("b", "2"), ("c", "3")]).await;
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        let mut request = ScanRequest::new(KeyRange::all());
        request.limit_rows = 2;
        let batch = txn.scan(request.clone()).await.unwrap();
        assert_eq!(batch.resume, Some(k("b")));
        let reads = txn.read_set();
        assert_eq!(
            reads.ranges(),
            [KeyRange::new(Bound::Unbounded, Bound::Included(k("b")))]
        );

        let batch = txn
            .scan(request.resume_after(&batch).unwrap())
            .await
            .unwrap();
        assert_eq!(batch.resume, None);
        assert_eq!(
            txn.read_set().ranges()[1],
            KeyRange::new(Bound::Excluded(k("b")), Bound::Unbounded)
        );
        txn.get_many(vec![k("z")]).await.unwrap();
        assert!(txn.read_set().keys().contains(&k("z")));
    }

    #[tokio::test]
    /// Assert rows rejected by a filter still count towards the covered range.
    async fn filtered_rows_are_still_read() {
        /// Reject every row.
        struct RejectAll;
        impl crate::RowFilter for RejectAll {
            /// Reject the row.
            fn eval(&self, _: &[u8], _: &[u8]) -> bool {
                false
            }
        }
        let (_dir, storage) = open();
        seed(&storage, &[("a", "1"), ("b", "2")]).await;
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        let mut request = ScanRequest::new(KeyRange::all());
        request.filter = Some(Arc::new(RejectAll));
        let batch = txn.scan(request).await.unwrap();
        assert_eq!(
            batch,
            ScanBatch {
                rows: vec![],
                resume: None
            }
        );
        assert_eq!(txn.read_set().ranges(), [KeyRange::all()]);
    }

    #[tokio::test]
    /// Assert commit refuses while clones are alive and succeeds from the last clone.
    async fn commit_requires_sole_handle() {
        let (_dir, storage) = open();
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        let clone = txn.clone();
        clone.put(k("a"), k("1")).unwrap();
        assert!(matches!(txn.commit().await, Err(Error::TransactionShared)));
        let info = clone.commit().await.unwrap();
        assert_eq!(info.commit_ts, Some(Timestamp::from_raw(1)));
        assert_eq!(storage.visible_ts(), Timestamp::from_raw(1));
    }

    #[tokio::test]
    /// Assert read-only transactions reject writes and empty commits use no timestamp.
    async fn read_only_and_empty_commits() {
        let (_dir, storage) = open();
        let txn = storage
            .begin(TxnOptions::default().read_only())
            .await
            .unwrap();
        assert!(matches!(
            txn.put(k("a"), k("1")),
            Err(Error::ReadOnlyTransaction)
        ));
        assert_eq!(txn.commit().await.unwrap().commit_ts, None);
        assert_eq!(storage.visible_ts(), Timestamp::ZERO);
    }

    #[tokio::test]
    /// Assert committed data and the commit clock survive a restart.
    async fn recovery_restores_clock_and_data() {
        let dir = tempfile::tempdir().unwrap();
        {
            let storage = FjallStorage::open(dir.path(), StorageOptions::default()).unwrap();
            seed(&storage, &[("a", "1")]).await;
            seed(&storage, &[("a", "2")]).await;
        }
        let storage = FjallStorage::open(dir.path(), StorageOptions::default()).unwrap();
        assert_eq!(storage.visible_ts(), Timestamp::from_raw(2));
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        assert_eq!(
            txn.get_many(vec![k("a")]).await.unwrap(),
            vec![Some(k("2"))]
        );
    }
}
