//! The cloneable transaction handle (ADR-0006).

use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use tracing::Instrument;

use super::read_set::ReadSet;
use super::write_buffer::{WriteBuffer, WriteOp};
use super::{Isolation, TxnOptions};
use crate::commit::{CommitRequest, SnapshotPin};
use crate::encoding::{EncodeError, MAX_TUPLE_LEN};
use crate::engine::mvcc::{self, ScanJob};
use crate::scan::{Key, KeyRange, ScanBatch, ScanRequest};
use crate::storage::Shared;
use crate::telemetry;
use crate::{Error, Result, Timestamp};

/// Outcome of a successful commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    /// The commit timestamp, or `None` if the transaction wrote nothing.
    pub commit_ts: Option<Timestamp>,
}

/// Identifies a savepoint within one transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SavepointId(u64);

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
    /// Keeps validation history and old versions alive for the first
    /// snapshot, until the maximum transaction age.
    pin: SnapshotPin,
    state: Mutex<TxnState>,
}

/// Mutable transaction state.
struct TxnState {
    snapshot: Timestamp,
    statement: u64,
    writes: WriteBuffer,
    reads: ReadSet,
    /// Reads validated at every isolation level.
    dependencies: ReadSet,
    /// Snapshot at which the first dependency was registered (the oldest,
    /// since snapshots only move forward).
    dependency_snapshot: Option<Timestamp>,
    /// Open savepoints, oldest first, with their undo-log positions.
    savepoints: Vec<(SavepointId, usize)>,
    next_savepoint: u64,
}

impl Txn {
    /// Construct a transaction reading at the current visible timestamp.
    pub(crate) fn begin(shared: Arc<Shared>, options: TxnOptions) -> Self {
        let pin = shared.clock.pin(shared.limits.max_transaction_age);
        let snapshot = pin.ts();
        Self(Arc::new(TxnInner {
            shared,
            options,
            pin,
            state: Mutex::new(TxnState {
                snapshot,
                statement: 0,
                writes: WriteBuffer::default(),
                reads: ReadSet::default(),
                dependencies: ReadSet::default(),
                dependency_snapshot: None,
                savepoints: Vec::new(),
                next_savepoint: 0,
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
        self.read_keys(keys, false).await
    }

    /// Read several keys like [`get_many`](Self::get_many), but also see
    /// writes from the current statement.
    ///
    /// Constraint checks and index maintenance need the latest state: two
    /// rows with the same key inserted by one statement must still collide.
    pub(crate) async fn get_many_latest(&self, keys: Vec<Key>) -> Result<Vec<Option<Bytes>>> {
        self.read_keys(keys, true).await
    }

    /// Read `keys` at the snapshot, overlaid with buffered writes from earlier
    /// statements, or from every statement if `latest`.
    async fn read_keys(&self, keys: Vec<Key>, latest: bool) -> Result<Vec<Option<Bytes>>> {
        self.check_age()?;
        let (snapshot, mut results, missing) = {
            let mut state = self.state();
            let statement = state.statement;
            let results: Vec<Option<Option<Bytes>>> = keys
                .iter()
                .map(|key| {
                    let buffered = if latest {
                        state.writes.latest(key)
                    } else {
                        state.writes.visible(key, statement)
                    };
                    buffered.map(|op| op.value().cloned())
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
                .run(move |engine| mvcc::read_many(&engine.data(), &missing_keys, snapshot))
                .await??;
            // GC may have removed versions this read needed once the pin expired.
            self.check_age()?;
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
        self.scan_with(request, false).await
    }

    /// Read one batch of each of several range scans in one storage call.
    ///
    /// Returns one batch per request, in order, exactly as [`scan`](Self::scan)
    /// would for each: continue a scan whose batch has a resume key with a
    /// request carrying it. Index lookups for a batch of keys use this to
    /// make one call instead of one per key.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage pool is unavailable or fjall fails.
    pub async fn scan_many(&self, requests: Vec<ScanRequest>) -> Result<Vec<ScanBatch>> {
        self.check_age()?;
        let jobs: Vec<Option<ScanJob>> = {
            let state = self.state();
            requests
                .into_iter()
                .map(|request| Self::scan_job(&state, request, false))
                .collect()
        };
        if jobs.iter().all(Option::is_none) {
            return Ok(jobs
                .iter()
                .map(|_| ScanBatch {
                    rows: Vec::new(),
                    resume: None,
                })
                .collect());
        }
        let outputs = self
            .0
            .shared
            .pool
            .run(move |engine| {
                jobs.into_iter()
                    .map(|job| job.map(|job| mvcc::scan(&engine.data(), job)).transpose())
                    .collect::<Result<Vec<_>>>()
            })
            .await??;
        // GC may have removed versions this scan needed once the pin expired.
        self.check_age()?;
        let mut state = self.state();
        Ok(outputs
            .into_iter()
            .map(|output| match output {
                Some(output) => {
                    state.reads.record_range(output.covered);
                    ScanBatch {
                        rows: output.rows,
                        resume: output.resume,
                    }
                }
                None => ScanBatch {
                    rows: Vec::new(),
                    resume: None,
                },
            })
            .collect())
    }

    /// Build the storage job for one scan batch, or `None` if the range left is empty.
    fn scan_job(state: &TxnState, request: ScanRequest, latest: bool) -> Option<ScanJob> {
        let range = match (request.resume, request.reverse) {
            (None, _) => request.range,
            (Some(resume), false) => KeyRange::new(Bound::Excluded(resume), request.range.end),
            (Some(resume), true) => KeyRange::new(request.range.start, Bound::Excluded(resume)),
        };
        if range.is_empty() {
            return None;
        }
        let statement = (!latest).then_some(state.statement);
        Some(ScanJob {
            buffered: state.writes.visible_range(&range, statement),
            range,
            reverse: request.reverse,
            limit_rows: request.limit_rows,
            limit_bytes: request.limit_bytes,
            filter: request.filter,
            snapshot: state.snapshot,
        })
    }

    /// Read one batch of a range scan like [`scan`](Self::scan), but also see
    /// writes from the current statement.
    pub(crate) async fn scan_latest(&self, request: ScanRequest) -> Result<ScanBatch> {
        self.scan_with(request, true).await
    }

    /// Read one batch of a range scan, merging buffered writes from earlier
    /// statements, or from every statement if `latest`.
    async fn scan_with(&self, request: ScanRequest, latest: bool) -> Result<ScanBatch> {
        self.check_age()?;
        let Some(job) = Self::scan_job(&self.state(), request, latest) else {
            return Ok(ScanBatch {
                rows: Vec::new(),
                resume: None,
            });
        };
        let output = self
            .0
            .shared
            .pool
            .run(move |engine| mvcc::scan(&engine.data(), job))
            .await??;
        // GC may have removed versions this scan needed once the pin expired.
        self.check_age()?;
        self.state().reads.record_range(output.covered);
        Ok(ScanBatch {
            rows: output.rows,
            resume: output.resume,
        })
    }

    /// Buffer a write setting `key` to `value`.
    ///
    /// Keys must follow the arity rule (see [`encoding`](crate::encoding)):
    /// no stored key may be a byte prefix of another stored key, which
    /// encoded tuples of one arity guarantee. The row layer always does;
    /// callers writing raw keys must too, or scans can skip keys.
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

    /// Register a key whose value this transaction depends on at every isolation level.
    ///
    /// Unlike [`register_read`](Self::register_read), which is validated
    /// only under SERIALIZABLE, the commit fails with
    /// [`Conflict::DependencyChanged`](crate::Conflict::DependencyChanged)
    /// if a transaction that committed after this one's snapshot wrote the
    /// key. The row layer uses this so that DML conflicts with concurrent
    /// DDL on the same table (ADR-0007).
    pub fn register_dependency(&self, key: Key) {
        let mut state = self.state();
        let snapshot = state.snapshot;
        state.dependency_snapshot.get_or_insert(snapshot);
        state.dependencies.record_key(key);
    }

    /// Register a range whose contents this transaction depends on at every isolation level.
    ///
    /// See [`register_dependency`](Self::register_dependency).
    pub fn register_dependency_range(&self, range: KeyRange) {
        let mut state = self.state();
        let snapshot = state.snapshot;
        state.dependency_snapshot.get_or_insert(snapshot);
        state.dependencies.record_range(range);
    }

    /// Open a savepoint at the current state of the write buffer.
    #[must_use]
    pub fn savepoint(&self) -> SavepointId {
        let mut state = self.state();
        let id = SavepointId(state.next_savepoint);
        state.next_savepoint += 1;
        let mark = state.writes.mark();
        state.savepoints.push((id, mark));
        id
    }

    /// Undo every write made since `savepoint` was opened.
    ///
    /// The savepoint stays open, so it can be rolled back to again; savepoints
    /// opened after it are released. Reads made since then stay in the read
    /// set, because the transaction may already have acted on them (ADR-0004).
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownSavepoint`] if the savepoint is not open.
    pub fn rollback_to(&self, savepoint: SavepointId) -> Result<()> {
        let mut state = self.state();
        let index = state.savepoint_index(savepoint)?;
        let mark = state.savepoints[index].1;
        state.savepoints.truncate(index + 1);
        state.writes.rollback_to(mark);
        Ok(())
    }

    /// Release `savepoint` and every savepoint opened after it, keeping their writes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownSavepoint`] if the savepoint is not open.
    pub fn release_savepoint(&self, savepoint: SavepointId) -> Result<()> {
        let mut state = self.state();
        let index = state.savepoint_index(savepoint)?;
        state.savepoints.truncate(index);
        if state.savepoints.is_empty() {
            state.writes.clear_undo();
        }
        Ok(())
    }

    /// Return the size of the transaction's buffered writes, in bytes of
    /// keys plus values: what its commit would write.
    #[must_use]
    pub fn write_set_bytes(&self) -> usize {
        self.state().writes.bytes()
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
    /// A transaction that wrote nothing commits without validation: its
    /// snapshot is already a consistent point in the commit order. Otherwise
    /// the commit sequencer validates it (ADR-0004) and returns once its
    /// commit group is written with the configured
    /// [`Durability`](crate::Durability).
    ///
    /// # Errors
    ///
    /// - [`Error::TransactionShared`] if other clones of this handle are still
    ///   alive. This handle is consumed; a remaining clone can commit later.
    /// - [`Error::SerializationFailure`] if validation fails; retry the
    ///   transaction.
    /// - [`Error::CommitOutcomeUnknown`] if writing the commit group failed.
    /// - [`Error::SequencerStopped`] or [`Error::TimestampExhausted`] if the
    ///   database can no longer accept commits.
    pub async fn commit(self) -> Result<CommitInfo> {
        let inner = Arc::try_unwrap(self.0).map_err(|_| Error::TransactionShared)?;
        if inner.pin.is_expired() {
            return Err(Error::TransactionTooOld {
                max_age: inner.shared.limits.max_transaction_age,
            });
        }
        let state = inner
            .state
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        if state.writes.is_empty() {
            return Ok(CommitInfo { commit_ts: None });
        }
        let reads = if inner.options.isolation == Isolation::Serializable {
            state.reads
        } else {
            ReadSet::default()
        };
        let request = CommitRequest {
            snapshot: state.snapshot,
            reads,
            dependencies: state.dependencies,
            dependency_snapshot: state.dependency_snapshot.unwrap_or(state.snapshot),
            writes: state.writes.into_intents(),
        };
        let bytes: usize = request
            .writes
            .iter()
            .map(|w| w.key.len() + w.op.value().map_or(0, Bytes::len))
            .sum();
        let isolation = inner.options.isolation.label();
        let durability = inner.shared.durability.label();
        let span = tracing::info_span!(
            "interlaken.commit",
            isolation,
            durability,
            outcome = tracing::field::Empty,
        );
        let started = Instant::now();
        // `inner` keeps the snapshot pin alive until validation is done.
        let result = inner
            .shared
            .commit
            .submit(request)
            .instrument(span.clone())
            .await;
        let outcome = match &result {
            Ok(_) => "committed",
            Err(Error::SerializationFailure(_)) => "conflict",
            Err(_) => "error",
        };
        span.record("outcome", outcome);
        metrics::counter!(telemetry::COMMITS, "isolation" => isolation, "outcome" => outcome)
            .increment(1);
        metrics::histogram!(telemetry::COMMIT_DURATION, "durability" => durability)
            .record(telemetry::seconds(started.elapsed()));
        let ts = result?;
        metrics::counter!(telemetry::BYTES_COMMITTED)
            .increment(u64::try_from(bytes).unwrap_or(u64::MAX));
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
        self.check_age()?;
        if key.len() > MAX_TUPLE_LEN {
            return Err(EncodeError::KeyTooLong {
                len: key.len(),
                max: MAX_TUPLE_LEN,
            }
            .into());
        }
        let mut state = self.state();
        let limit = self.0.shared.limits.max_write_set_bytes;
        if state.writes.bytes_after(&key, &op) > limit {
            return Err(Error::WriteSetTooLarge { limit });
        }
        let (statement, snapshot) = (state.statement, state.snapshot);
        state.writes.write(key, op, statement, snapshot);
        Ok(())
    }

    /// Fail if the transaction has exceeded the maximum age.
    fn check_age(&self) -> Result<()> {
        if self.0.pin.is_expired() {
            return Err(Error::TransactionTooOld {
                max_age: self.0.shared.limits.max_transaction_age,
            });
        }
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

impl TxnState {
    /// Return the stack position of an open savepoint.
    fn savepoint_index(&self, savepoint: SavepointId) -> Result<usize> {
        self.savepoints
            .iter()
            .position(|(id, _)| *id == savepoint)
            .ok_or(Error::UnknownSavepoint)
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
            durability: crate::Durability::Buffered,
            ..StorageOptions::default()
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

    /// Begin a read-write transaction at `isolation`.
    async fn begin(storage: &FjallStorage, isolation: Isolation) -> Txn {
        storage.begin(TxnOptions::new(isolation)).await.unwrap()
    }

    /// Return the conflict a commit failed with.
    async fn conflict_of(txn: Txn) -> Option<crate::Conflict> {
        match txn.commit().await {
            Ok(_) => None,
            Err(Error::SerializationFailure(conflict)) => Some(conflict),
            Err(other) => panic!("unexpected error {other}"),
        }
    }

    #[tokio::test]
    /// Assert concurrent writes to one key abort the second committer at every level.
    async fn first_committer_wins_at_every_level() {
        for isolation in [
            Isolation::ReadCommitted,
            Isolation::RepeatableRead,
            Isolation::Serializable,
        ] {
            let (_dir, storage) = open();
            let a = begin(&storage, isolation).await;
            let b = begin(&storage, isolation).await;
            a.put(k("x"), k("a")).unwrap();
            b.put(k("x"), k("b")).unwrap();
            assert_eq!(conflict_of(a).await, None);
            assert_eq!(
                conflict_of(b).await,
                Some(crate::Conflict::WriteWrite),
                "{isolation:?}"
            );
        }
    }

    #[tokio::test]
    /// Assert a READ COMMITTED write after a snapshot refresh doesn't conflict with what it saw.
    async fn read_committed_write_after_refresh() {
        let (_dir, storage) = open();
        let rc = begin(&storage, Isolation::ReadCommitted).await;
        seed(&storage, &[("x", "other")]).await;
        rc.next_statement();
        rc.put(k("x"), k("rc")).unwrap();
        assert_eq!(conflict_of(rc).await, None);

        // A write made before the refresh still conflicts.
        let rc = begin(&storage, Isolation::ReadCommitted).await;
        rc.put(k("x"), k("rc")).unwrap();
        seed(&storage, &[("x", "other")]).await;
        rc.next_statement();
        rc.put(k("x"), k("rc2")).unwrap();
        assert_eq!(conflict_of(rc).await, Some(crate::Conflict::WriteWrite));
    }

    #[tokio::test]
    /// Assert write skew commits under REPEATABLE READ but aborts under SERIALIZABLE.
    async fn write_skew() {
        for (isolation, expected) in [
            (Isolation::RepeatableRead, None),
            (Isolation::Serializable, Some(crate::Conflict::ReadWrite)),
        ] {
            let (_dir, storage) = open();
            seed(&storage, &[("x", "1"), ("y", "1")]).await;
            let a = begin(&storage, isolation).await;
            let b = begin(&storage, isolation).await;
            a.get_many(vec![k("x"), k("y")]).await.unwrap();
            b.get_many(vec![k("x"), k("y")]).await.unwrap();
            a.put(k("x"), k("0")).unwrap();
            b.put(k("y"), k("0")).unwrap();
            assert_eq!(conflict_of(a).await, None);
            assert_eq!(conflict_of(b).await, expected, "{isolation:?}");
        }
    }

    #[tokio::test]
    /// Assert an insert into a scanned range aborts a SERIALIZABLE reader that writes.
    async fn phantom_insert_conflicts() {
        let (_dir, storage) = open();
        let reader = begin(&storage, Isolation::Serializable).await;
        let rows = reader
            .scan(ScanRequest::new(KeyRange::prefix(b"p")))
            .await
            .unwrap()
            .rows;
        assert!(rows.is_empty());
        seed(&storage, &[("p1", "new")]).await;
        reader.put(k("summary"), k("0")).unwrap();
        assert_eq!(conflict_of(reader).await, Some(crate::Conflict::ReadWrite));

        // An insert outside the covered range doesn't conflict.
        let reader = begin(&storage, Isolation::Serializable).await;
        reader
            .scan(ScanRequest::new(KeyRange::prefix(b"p")))
            .await
            .unwrap();
        seed(&storage, &[("q1", "new")]).await;
        reader.put(k("summary"), k("0")).unwrap();
        assert_eq!(conflict_of(reader).await, None);
    }

    #[tokio::test]
    /// Assert transactions that only read never abort.
    async fn readers_never_abort() {
        let (_dir, storage) = open();
        let read_write = begin(&storage, Isolation::Serializable).await;
        let read_only = storage
            .begin(TxnOptions::default().read_only())
            .await
            .unwrap();
        read_write.get_many(vec![k("x")]).await.unwrap();
        read_only.get_many(vec![k("x")]).await.unwrap();
        seed(&storage, &[("x", "1")]).await;
        assert_eq!(read_write.commit().await.unwrap().commit_ts, None);
        assert_eq!(read_only.commit().await.unwrap().commit_ts, None);
    }

    #[tokio::test]
    /// Assert rollback undoes writes but keeps reads, and nested savepoints are released.
    async fn savepoints() {
        let (_dir, storage) = open();
        seed(&storage, &[("r", "1")]).await;
        let txn = begin(&storage, Isolation::Serializable).await;
        txn.put(k("a"), k("1")).unwrap();
        let outer = txn.savepoint();
        txn.put(k("a"), k("2")).unwrap();
        txn.next_statement();
        let inner = txn.savepoint();
        txn.get_many(vec![k("r")]).await.unwrap();
        txn.put(k("b"), k("3")).unwrap();

        txn.rollback_to(outer).unwrap();
        assert!(matches!(
            txn.rollback_to(inner),
            Err(Error::UnknownSavepoint)
        ));
        txn.next_statement();
        assert_eq!(
            txn.get_many(vec![k("a"), k("b")]).await.unwrap(),
            vec![Some(k("1")), None]
        );
        assert!(
            txn.read_set().keys().contains(&k("r")),
            "reads survive rollback"
        );

        // The savepoint stays open after a rollback.
        txn.put(k("c"), k("4")).unwrap();
        txn.rollback_to(outer).unwrap();
        txn.release_savepoint(outer).unwrap();
        assert!(matches!(
            txn.release_savepoint(outer),
            Err(Error::UnknownSavepoint)
        ));
        txn.commit().await.unwrap();

        let check = storage
            .begin(TxnOptions::default().read_only())
            .await
            .unwrap();
        assert_eq!(
            check.get_many(vec![k("a"), k("b"), k("c")]).await.unwrap(),
            vec![Some(k("1")), None, None]
        );
    }

    #[tokio::test]
    /// Assert a rolled-back write no longer conflicts.
    async fn rolled_back_write_does_not_conflict() {
        let (_dir, storage) = open();
        let txn = begin(&storage, Isolation::RepeatableRead).await;
        txn.put(k("keep"), k("1")).unwrap();
        let sp = txn.savepoint();
        txn.put(k("x"), k("1")).unwrap();
        txn.rollback_to(sp).unwrap();
        seed(&storage, &[("x", "other")]).await;
        assert_eq!(conflict_of(txn).await, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    /// Assert concurrent commits get distinct consecutive timestamps under both durability modes.
    async fn concurrent_commits_get_consecutive_timestamps() {
        for durability in [crate::Durability::Sync, crate::Durability::Buffered] {
            let dir = tempfile::tempdir().unwrap();
            let options = StorageOptions {
                durability,
                max_commit_group: 8,
                ..StorageOptions::default()
            };
            let storage = FjallStorage::open(dir.path(), options).unwrap();
            let tasks: Vec<_> = (0..64u32)
                .map(|i| {
                    let storage = storage.clone();
                    tokio::spawn(async move {
                        let txn = storage.begin(TxnOptions::default()).await.unwrap();
                        txn.put(Bytes::from(i.to_be_bytes().to_vec()), Bytes::new())
                            .unwrap();
                        txn.commit().await.unwrap().commit_ts.unwrap().into_raw()
                    })
                })
                .collect();
            let mut stamps = Vec::new();
            for task in tasks {
                stamps.push(task.await.unwrap());
            }
            stamps.sort_unstable();
            assert_eq!(stamps, (1..=64).collect::<Vec<_>>());
            assert_eq!(storage.visible_ts(), Timestamp::from_raw(64));
        }
    }
}
