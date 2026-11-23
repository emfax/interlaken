//! Opening the fjall database and its keyspaces.

use std::path::Path;

use bytes::Bytes;
use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};

use super::meta::{
    FORMAT_VERSION, FORMAT_VERSION_KEY, GC_WATERMARK_KEY, LAST_APPLIED_INDEX_KEY,
    LAST_COMMIT_TS_KEY, decode_format_version, decode_timestamp, encode_format_version,
    encode_timestamp,
};
use super::stored::{StoredValue, decode_stored, encode_stored};
use crate::commit::LogEntry;
use crate::encoding::{split_versioned_key, versioned_key};
use crate::{Durability, Error, Result, Timestamp};

/// Default cap on fjall's journals: fjall's own default, 512 MiB.
pub(crate) const DEFAULT_MAX_JOURNAL_BYTES: u64 = 512 * 1024 * 1024;
/// Smallest journal cap fjall accepts, 64 MiB.
pub(crate) const MIN_MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
/// Default memtable size per keyspace: fjall's own default, 64 MiB.
pub(crate) const DEFAULT_MEMTABLE_BYTES: u64 = 64 * 1024 * 1024;
/// Smallest memtable size the engine configures, 1 MiB.
pub(crate) const MIN_MEMTABLE_BYTES: u64 = 1024 * 1024;

/// Name of the versioned keyspace holding rows, index entries and the catalog.
const DATA_KEYSPACE: &str = "data";
/// Name of the unversioned keyspace holding engine state.
const META_KEYSPACE: &str = "meta";
/// Name of the keyspace holding commit log entries by index (ADR-0037).
const LOG_KEYSPACE: &str = "raftlog";
/// Name of the keyspace holding a replica's Raft log entries by index (ADR-0037).
const REPLICA_LOG_KEYSPACE: &str = "replica_log";
/// Name of the keyspace holding a replica's latest snapshot, in chunks (ADR-0037).
const REPLICA_SNAPSHOT_KEYSPACE: &str = "replica_snapshot";
/// Size of each stored snapshot chunk.
const SNAPSHOT_CHUNK_BYTES: usize = 1024 * 1024;
/// Versions written per batch when importing a snapshot.
const IMPORT_BATCH: usize = 4096;
/// The `meta` key of the stored snapshot's generation.
const SNAPSHOT_GENERATION_KEY: &[u8] = b"replica_snapshot_generation";
/// The `meta` key present while a snapshot import is incomplete.
const IMPORTING_KEY: &[u8] = b"replica_importing";
/// The first bytes of an exported snapshot.
const SNAPSHOT_MAGIC: &[u8] = b"ILSNAP2\0";
/// The marker before an exported snapshot's entry count.
const SNAPSHOT_END: &[u8] = b"ILSNEND\0";

/// The fjall database with its `data`, `meta` and `raftlog` keyspaces.
///
/// All methods are synchronous and call fjall directly. Per ADR-0001 they
/// must only run on storage threads, never inside an async task.
///
/// Opening a fresh directory initialises `meta` with the current
/// [`FORMAT_VERSION`], a last commit timestamp of [`Timestamp::ZERO`] and a GC
/// watermark of [`Timestamp::ZERO`], durably, before returning.
#[derive(Clone)]
pub struct Engine {
    db: Database,
    data: Keyspace,
    meta: Keyspace,
    log: Keyspace,
    replica_log: Keyspace,
    replica_snapshot: Keyspace,
}

impl Engine {
    /// Open or create a database in the directory at `path`.
    ///
    /// # Errors
    ///
    /// - [`Error::UnsupportedFormatVersion`] if the directory was written by
    ///   an incompatible build.
    /// - [`Error::CorruptMeta`] if `meta` is missing entries or holds
    ///   malformed values, including a populated database with no format version.
    /// - [`Error::Fjall`] if fjall fails to open or write.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_storage::{Engine, FORMAT_VERSION, Timestamp};
    ///
    /// let dir = tempfile::tempdir()?;
    /// let engine = Engine::open(dir.path())?;
    /// assert_eq!(engine.format_version()?, FORMAT_VERSION);
    /// assert_eq!(engine.last_commit_ts()?, Timestamp::ZERO);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, DEFAULT_MAX_JOURNAL_BYTES, DEFAULT_MEMTABLE_BYTES)
    }

    /// Open or create a database with explicit journal and memtable limits.
    ///
    /// `max_journal_bytes` caps the journals fjall keeps before it flushes
    /// memtables so the oldest journal can be deleted (raised to at least
    /// [`MIN_MAX_JOURNAL_BYTES`]). `memtable_bytes` is the size at which a
    /// keyspace's memtable is flushed to a table (raised to at least
    /// [`MIN_MEMTABLE_BYTES`]); fjall records it when a keyspace is created,
    /// so it has no effect on an existing database. Smaller values shrink
    /// the steady-state footprint at the cost of more flushes and compaction.
    ///
    /// # Errors
    ///
    /// As [`Engine::open`].
    pub(crate) fn open_with(
        path: impl AsRef<Path>,
        max_journal_bytes: u64,
        memtable_bytes: u64,
    ) -> Result<Self> {
        let memtable_bytes = memtable_bytes.max(MIN_MEMTABLE_BYTES);
        let db = Database::builder(path.as_ref())
            .max_journaling_size(max_journal_bytes.max(MIN_MAX_JOURNAL_BYTES))
            .open()?;
        let create = || KeyspaceCreateOptions::default().max_memtable_size(memtable_bytes);
        let data = db.keyspace(DATA_KEYSPACE, create)?;
        let meta = db.keyspace(META_KEYSPACE, create)?;
        let log = db.keyspace(LOG_KEYSPACE, create)?;
        let replica_log = db.keyspace(REPLICA_LOG_KEYSPACE, create)?;
        let replica_snapshot = db.keyspace(REPLICA_SNAPSHOT_KEYSPACE, create)?;
        let engine = Self {
            db,
            data,
            meta,
            log,
            replica_log,
            replica_snapshot,
        };

        match engine.meta.get(FORMAT_VERSION_KEY)? {
            Some(value) => engine.validate_meta(&value)?,
            None => engine.initialise_meta()?,
        }
        Ok(engine)
    }

    /// Return the on-disk format version recorded in `meta`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptMeta`] if the entry is missing or malformed, or
    /// [`Error::Fjall`] if the read fails.
    pub fn format_version(&self) -> Result<u32> {
        decode_format_version(&self.read_meta(FORMAT_VERSION_KEY, "format_version missing")?)
    }

    /// Return the last commit timestamp recorded in `meta`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptMeta`] if the entry is missing or malformed, or
    /// [`Error::Fjall`] if the read fails.
    pub fn last_commit_ts(&self) -> Result<Timestamp> {
        let value = self.read_meta(LAST_COMMIT_TS_KEY, "last_commit_ts missing")?;
        decode_timestamp(&value, "last_commit_ts is not 8 bytes")
    }

    /// Return the GC watermark recorded in `meta`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptMeta`] if the entry is missing or malformed, or
    /// [`Error::Fjall`] if the read fails.
    pub fn gc_watermark(&self) -> Result<Timestamp> {
        let value = self.read_meta(GC_WATERMARK_KEY, "gc_watermark missing")?;
        decode_timestamp(&value, "gc_watermark is not 8 bytes")
    }

    /// Return the fjall database.
    #[cfg(test)]
    pub(crate) fn database(&self) -> &Database {
        &self.db
    }

    /// Return the versioned `data` keyspace.
    pub(crate) fn data(&self) -> &Keyspace {
        &self.data
    }

    /// Write a group of commits as a single atomic fjall batch (ADR-0005).
    ///
    /// Each write `(ts, key, value)` becomes a version of `key` at `ts` (a
    /// tombstone when the value is `None`), and `last_commit_ts` in `meta` is
    /// set to `last` in the same batch, so recovery never sees a partial group
    /// or a timestamp whose data is missing. Under [`Durability::Sync`] the
    /// journal is synced with `SyncAll` before this returns; under
    /// [`Durability::Buffered`] it is flushed to OS buffers (`Buffer`), so the
    /// group survives a process crash. Without a persist mode fjall leaves
    /// the batch in its in-process journal buffer, where a crash loses it:
    /// the F4 crash harness caught exactly that (ADR-0005).
    pub(crate) fn commit_group<'a>(
        &self,
        last: Timestamp,
        writes: impl IntoIterator<Item = (Timestamp, &'a [u8], Option<&'a [u8]>)>,
        durability: Durability,
    ) -> Result<()> {
        let mode = match durability {
            Durability::Sync => PersistMode::SyncAll,
            Durability::Buffered => PersistMode::Buffer,
        };
        let mut batch = self.db.batch().durability(Some(mode));
        for (ts, key, value) in writes {
            batch.insert(&self.data, versioned_key(key, ts), encode_stored(value));
        }
        batch.insert(&self.meta, LAST_COMMIT_TS_KEY, encode_timestamp(last));
        batch.commit()?;
        Ok(())
    }

    /// Return the index of the last commit log entry applied to `data`; 0
    /// if the database never used the commit log.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptMeta`] if the entry is malformed, or
    /// [`Error::Fjall`] if the read fails.
    pub(crate) fn last_applied_index(&self) -> Result<u64> {
        match self.meta.get(LAST_APPLIED_INDEX_KEY)? {
            None => Ok(0),
            Some(value) => {
                Ok(decode_timestamp(&value, "last_applied_index is not 8 bytes")?.into_raw())
            }
        }
    }

    /// Append `entry` to the commit log as one batch, synced as `durability`
    /// says (ADR-0037). Once this returns, the entry is committed: if the
    /// process stops before it is applied, opening the database applies it.
    pub(crate) fn append_log(&self, entry: &LogEntry, durability: Durability) -> Result<()> {
        let mode = match durability {
            Durability::Sync => PersistMode::SyncAll,
            Durability::Buffered => PersistMode::Buffer,
        };
        let mut batch = self.db.batch().durability(Some(mode));
        batch.insert(&self.log, entry.index.to_be_bytes(), entry.encode());
        batch.commit()?;
        Ok(())
    }

    /// Apply a committed log entry: write its versions to `data`, and set
    /// `last_commit_ts` and `last_applied_index` in `meta`, in one batch.
    ///
    /// The batch isn't synced: the log entry is the durable record, and
    /// replaying it on open restores anything a crash loses.
    pub(crate) fn apply_entry(&self, entry: &LogEntry) -> Result<()> {
        let mut batch = self.db.batch();
        for (ts, write) in entry.versions() {
            batch.insert(
                &self.data,
                versioned_key(&write.key, ts),
                encode_stored(write.value.as_deref()),
            );
        }
        batch.insert(
            &self.meta,
            LAST_COMMIT_TS_KEY,
            encode_timestamp(entry.last_ts()),
        );
        batch.insert(
            &self.meta,
            LAST_APPLIED_INDEX_KEY,
            encode_timestamp(Timestamp::from_raw(entry.index)),
        );
        batch.commit()?;
        Ok(())
    }

    /// Apply every log entry after the last applied one, in order, and
    /// return the last commit timestamp and the last log index.
    ///
    /// Opening a database calls this whichever commit path it then uses,
    /// so entries committed to the log before a crash are never lost.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptMeta`] for a malformed entry or a gap in the
    /// log, or [`Error::Fjall`] if a read or write fails.
    pub(crate) fn replay_log(&self) -> Result<(Timestamp, u64)> {
        let applied = self.last_applied_index()?;
        let mut last_index = applied;
        for item in self.log.range((applied + 1).to_be_bytes()..) {
            let entry = LogEntry::decode(&item.value()?)?;
            if entry.index != last_index + 1 {
                return Err(Error::CorruptMeta("gap in the commit log"));
            }
            self.apply_entry(&entry)?;
            last_index = entry.index;
        }
        // The highest index ever logged, so new entries never reuse one.
        let logged = self
            .log
            .last_key_value()
            .map(|item| item.key())
            .transpose()?
            .map_or(Ok::<u64, Error>(0), |key| {
                let bytes: [u8; 8] = key[..]
                    .try_into()
                    .map_err(|_| Error::CorruptMeta("commit log key is not 8 bytes"))?;
                Ok(u64::from_be_bytes(bytes))
            })?;
        Ok((self.last_commit_ts()?, last_index.max(logged)))
    }

    /// Sync everything written so far, then remove the log entries up to
    /// and including `through`, whose versions are then durable in `data`.
    pub(crate) fn compact_log(&self, through: u64) -> Result<()> {
        self.db.persist(PersistMode::SyncAll)?;
        let mut batch = self.db.batch();
        for item in self.log.range(..=through.to_be_bytes()) {
            batch.remove(&self.log, item.key()?);
        }
        batch.commit()?;
        Ok(())
    }

    /// Write versions to `data`, with `last_commit_ts` set to `last` and the
    /// `meta` entries given, in one unsynced batch: a replica applying a
    /// committed Raft entry, whose log is the durable record (ADR-0037).
    pub(crate) fn apply_versions<'a>(
        &self,
        writes: impl IntoIterator<Item = (Timestamp, &'a [u8], Option<&'a [u8]>)>,
        last: Option<Timestamp>,
        meta: &[(Vec<u8>, Vec<u8>)],
    ) -> Result<()> {
        let mut batch = self.db.batch();
        for (ts, key, value) in writes {
            batch.insert(&self.data, versioned_key(key, ts), encode_stored(value));
        }
        if let Some(last) = last {
            batch.insert(&self.meta, LAST_COMMIT_TS_KEY, encode_timestamp(last));
        }
        for (key, value) in meta {
            batch.insert(&self.meta, key.as_slice(), value.as_slice());
        }
        batch.commit()?;
        Ok(())
    }

    /// Return the `meta` entry at `key`, if any.
    pub(crate) fn get_meta(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.meta.get(key)?.map(|v| v.to_vec()))
    }

    /// Set `meta` entries in one batch, synced if `sync`.
    pub(crate) fn set_meta(&self, entries: &[(Vec<u8>, Vec<u8>)], sync: bool) -> Result<()> {
        let mut batch = self
            .db
            .batch()
            .durability(sync.then_some(PersistMode::SyncAll));
        for (key, value) in entries {
            batch.insert(&self.meta, key.as_slice(), value.as_slice());
        }
        batch.commit()?;
        Ok(())
    }

    /// Append entries to the replica log in one batch, synced if `sync`.
    pub(crate) fn replica_log_append(&self, entries: &[(u64, Vec<u8>)], sync: bool) -> Result<()> {
        let mut batch = self.db.batch().durability(Some(if sync {
            PersistMode::SyncAll
        } else {
            PersistMode::Buffer
        }));
        for (index, value) in entries {
            batch.insert(&self.replica_log, index.to_be_bytes(), value.as_slice());
        }
        batch.commit()?;
        Ok(())
    }

    /// Return the replica log entries with indexes in `start..end`, in order.
    pub(crate) fn replica_log_range(&self, start: u64, end: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        if start >= end {
            return Ok(Vec::new());
        }
        self.replica_log
            .range(start.to_be_bytes()..end.to_be_bytes())
            .map(|item| {
                let (key, value) = item.into_inner()?;
                Ok((log_index(&key)?, value.to_vec()))
            })
            .collect()
    }

    /// Return the last replica log entry, if any.
    pub(crate) fn replica_log_last(&self) -> Result<Option<(u64, Vec<u8>)>> {
        self.replica_log
            .last_key_value()
            .map(|item| {
                let (key, value) = item.into_inner()?;
                Ok((log_index(&key)?, value.to_vec()))
            })
            .transpose()
    }

    /// Return the first replica log entry, if any.
    pub(crate) fn replica_log_first(&self) -> Result<Option<(u64, Vec<u8>)>> {
        self.replica_log
            .first_key_value()
            .map(|item| {
                let (key, value) = item.into_inner()?;
                Ok((log_index(&key)?, value.to_vec()))
            })
            .transpose()
    }

    /// Remove the replica log entries with indexes in `start..end`, synced
    /// if `sync`.
    pub(crate) fn replica_log_remove(&self, start: u64, end: u64, sync: bool) -> Result<()> {
        let mut batch = self
            .db
            .batch()
            .durability(sync.then_some(PersistMode::SyncAll));
        for item in self
            .replica_log
            .range(start.to_be_bytes()..end.to_be_bytes())
        {
            batch.remove(&self.replica_log, item.key()?);
        }
        batch.commit()?;
        Ok(())
    }

    /// Return a consistent view of every keyspace, for exporting a snapshot.
    pub(crate) fn view(&self) -> fjall::Snapshot {
        self.db.snapshot()
    }

    /// Encode the current data in `view` as one snapshot (ADR-0038): the
    /// newest version of each key at or below the view's last commit, with
    /// its commit timestamp, leaving out keys whose newest version is a
    /// deletion, plus the last commit timestamp and the `meta` entries at
    /// `meta_keys`.
    ///
    /// Encoding (version 2), integers big-endian: `magic last_commit_ts:u64
    /// meta_count:u32 (bytes bytes)* (bytes commit_ts:u64 bytes)* end
    /// entry_count:u64`, where `bytes := len:u32 byte*`, the entries are
    /// keys, commit timestamps and values in key order, and `end` is a fixed
    /// marker. The trailer makes a snapshot cut short at an entry boundary
    /// detectable.
    pub(crate) fn export(&self, view: &fjall::Snapshot, meta_keys: &[Vec<u8>]) -> Result<Vec<u8>> {
        use fjall::Readable as _;
        let meta = |key: &[u8]| -> Result<Option<Vec<u8>>> {
            Ok(view.get(&self.meta, key)?.map(|v| v.to_vec()))
        };
        let last = meta(LAST_COMMIT_TS_KEY)?
            .as_deref()
            .map(|v| decode_timestamp(v, "snapshot timestamp"))
            .transpose()?
            .ok_or(Error::CorruptMeta("snapshot view lacks a timestamp"))?;
        let mut out = SNAPSHOT_MAGIC.to_vec();
        out.extend_from_slice(&last.into_raw().to_be_bytes());
        let present: Vec<(&Vec<u8>, Vec<u8>)> = meta_keys
            .iter()
            .filter_map(|key| meta(key).transpose().map(|v| v.map(|v| (key, v))))
            .collect::<Result<_>>()?;
        out.extend_from_slice(
            &u32::try_from(present.len())
                .expect("meta count fits u32")
                .to_be_bytes(),
        );
        let put = |out: &mut Vec<u8>, bytes: &[u8]| {
            out.extend_from_slice(
                &u32::try_from(bytes.len())
                    .expect("length fits u32")
                    .to_be_bytes(),
            );
            out.extend_from_slice(bytes);
        };
        for (key, value) in &present {
            put(&mut out, key);
            put(&mut out, value);
        }
        // Versions of a key sort together, newest first: keep the first one
        // at or below the snapshot, and skip the rest of the key's versions.
        let mut decided: Option<Vec<u8>> = None;
        let mut entries: u64 = 0;
        for item in view.iter(&self.data) {
            let (versioned, value) = item.into_inner()?;
            let (key, ts) = split_versioned_key(&versioned)?;
            if decided.as_deref() == Some(key) || ts > last {
                continue;
            }
            decided = Some(key.to_vec());
            if let StoredValue::Put(value) = decode_stored(Bytes::copy_from_slice(&value))? {
                put(&mut out, key);
                out.extend_from_slice(&ts.into_raw().to_be_bytes());
                put(&mut out, &value);
                entries += 1;
            }
        }
        out.extend_from_slice(SNAPSHOT_END);
        out.extend_from_slice(&entries.to_be_bytes());
        Ok(out)
    }

    /// Replace `data` with a snapshot's current data, writing each version
    /// at its original commit timestamp, and set the snapshot's last commit
    /// timestamp, its `meta` entries, and a GC watermark of at least its last
    /// commit: no older versions exist here any more. Return the snapshot's
    /// last commit timestamp.
    ///
    /// The whole snapshot is decoded before anything changes. A marker in
    /// `meta`, synced first and removed in the last batch, shows an import
    /// a crash interrupted ([`Engine::import_pending`]).
    pub(crate) fn import(&self, snapshot: &[u8]) -> Result<Timestamp> {
        let mut reader = SnapshotReader(
            snapshot
                .strip_prefix(SNAPSHOT_MAGIC)
                .ok_or_else(malformed_snapshot)?,
        );
        let last = Timestamp::from_raw(reader.u64()?);
        let meta_count = reader.u32()?;
        let mut meta = Vec::with_capacity(meta_count);
        for _ in 0..meta_count {
            let key = reader.bytes()?.to_vec();
            meta.push((key, reader.bytes()?.to_vec()));
        }
        let mut versions = Vec::new();
        let trailer = SNAPSHOT_END.len() + 8;
        loop {
            if reader.0.len() == trailer && reader.0.starts_with(SNAPSHOT_END) {
                reader.take(SNAPSHOT_END.len())?;
                let count = usize::try_from(reader.u64()?).map_err(|_| malformed_snapshot())?;
                if count != versions.len() {
                    return Err(malformed_snapshot());
                }
                break;
            }
            let key = reader.bytes()?;
            let ts = Timestamp::from_raw(reader.u64()?);
            if ts > last {
                return Err(malformed_snapshot());
            }
            versions.push((versioned_key(key, ts), reader.bytes()?));
        }
        let mut marker = self.db.batch().durability(Some(PersistMode::SyncAll));
        marker.insert(&self.meta, IMPORTING_KEY, b"1");
        marker.commit()?;
        self.data.clear()?;
        for chunk in versions.chunks(IMPORT_BATCH) {
            let mut batch = self.db.batch();
            for (key, value) in chunk {
                batch.insert(&self.data, key.as_slice(), encode_stored(Some(value)));
            }
            batch.commit()?;
        }
        let watermark = self.gc_watermark()?.max(last);
        let mut batch = self.db.batch();
        batch.insert(&self.meta, LAST_COMMIT_TS_KEY, encode_timestamp(last));
        batch.insert(&self.meta, GC_WATERMARK_KEY, encode_timestamp(watermark));
        for (key, value) in &meta {
            batch.insert(&self.meta, key.as_slice(), value.as_slice());
        }
        batch.remove(&self.meta, IMPORTING_KEY);
        batch.commit()?;
        self.persist()?;
        Ok(last)
    }

    /// Return `true` if a snapshot import was interrupted and must be redone.
    pub(crate) fn import_pending(&self) -> Result<bool> {
        Ok(self.meta.get(IMPORTING_KEY)?.is_some())
    }

    /// Replace the stored snapshot with `blob`, durably.
    ///
    /// Each snapshot is stored under a new generation, chunk keys being the
    /// generation and chunk index, and becomes current when the `meta`
    /// pointer to its generation is synced. Readers never see a snapshot
    /// half-replaced. Older generations are removed afterwards.
    pub(crate) fn store_snapshot(&self, blob: &[u8]) -> Result<()> {
        let current = self.snapshot_generation(&self.view())?;
        let generation = current.map_or(0, |g| g + 1);
        let key = |chunk: u32| {
            let mut key = generation.to_be_bytes().to_vec();
            key.extend_from_slice(&chunk.to_be_bytes());
            key
        };
        for (i, chunk) in blob.chunks(SNAPSHOT_CHUNK_BYTES).enumerate() {
            let mut batch = self.db.batch();
            batch.insert(
                &self.replica_snapshot,
                key(u32::try_from(i).expect("chunk count fits u32")),
                chunk,
            );
            batch.commit()?;
        }
        let mut batch = self.db.batch().durability(Some(PersistMode::SyncAll));
        batch.insert(
            &self.meta,
            SNAPSHOT_GENERATION_KEY,
            generation.to_be_bytes(),
        );
        batch.commit()?;
        let mut batch = self.db.batch();
        for item in self.replica_snapshot.range(..generation.to_be_bytes()) {
            batch.remove(&self.replica_snapshot, item.key()?);
        }
        batch.commit()?;
        Ok(())
    }

    /// Return the current snapshot generation in `view`, if any.
    fn snapshot_generation(&self, view: &fjall::Snapshot) -> Result<Option<u64>> {
        use fjall::Readable as _;
        view.get(&self.meta, SNAPSHOT_GENERATION_KEY)?
            .map(|v| {
                let bytes: [u8; 8] = v[..]
                    .try_into()
                    .map_err(|_| Error::CorruptMeta("snapshot generation"))?;
                Ok(u64::from_be_bytes(bytes))
            })
            .transpose()
    }

    /// Return the `meta` entry at `key` as `view` sees it.
    pub(crate) fn view_meta(&self, view: &fjall::Snapshot, key: &[u8]) -> Result<Option<Vec<u8>>> {
        use fjall::Readable as _;
        Ok(view.get(&self.meta, key)?.map(|v| v.to_vec()))
    }

    /// Remove the stored snapshot, if any.
    pub(crate) fn clear_stored_snapshot(&self) -> Result<()> {
        let mut batch = self.db.batch();
        batch.remove(&self.meta, SNAPSHOT_GENERATION_KEY);
        for item in self.replica_snapshot.iter() {
            batch.remove(&self.replica_snapshot, item.key()?);
        }
        batch.commit()?;
        Ok(())
    }

    /// Return the stored snapshot, if any.
    pub(crate) fn stored_snapshot(&self) -> Result<Option<Vec<u8>>> {
        use fjall::Readable as _;
        let view = self.view();
        let Some(generation) = self.snapshot_generation(&view)? else {
            return Ok(None);
        };
        let mut blob = Vec::new();
        for item in view.prefix(&self.replica_snapshot, generation.to_be_bytes()) {
            blob.extend_from_slice(&item.value()?);
        }
        Ok(Some(blob))
    }

    /// Sync everything written so far.
    pub(crate) fn persist(&self) -> Result<()> {
        self.db.persist(PersistMode::SyncAll)?;
        Ok(())
    }

    /// Physically remove versioned keys from `data` in one unsynced batch (ADR-0008).
    pub(crate) fn remove_versions(&self, keys: Vec<Vec<u8>>) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut batch = self.db.batch();
        for key in keys {
            batch.remove(&self.data, key);
        }
        batch.commit()?;
        Ok(())
    }

    /// Record the watermark of a completed GC pass in `meta`.
    pub(crate) fn set_gc_watermark(&self, watermark: Timestamp) -> Result<()> {
        self.meta
            .insert(GC_WATERMARK_KEY, encode_timestamp(watermark))?;
        Ok(())
    }

    /// Return the space the database uses on disk, in bytes.
    pub(crate) fn disk_space(&self) -> Result<u64> {
        Ok(self.db.disk_space()?)
    }

    /// Validate an existing `meta` keyspace against this build.
    fn validate_meta(&self, format_value: &[u8]) -> Result<()> {
        let found = decode_format_version(format_value)?;
        if found != FORMAT_VERSION {
            return Err(Error::UnsupportedFormatVersion {
                found,
                supported: FORMAT_VERSION,
            });
        }
        self.last_commit_ts()?;
        self.gc_watermark()?;
        Ok(())
    }

    /// Write the initial `meta` entries for a fresh database and sync them to disk.
    ///
    /// Refuses to initialise if either keyspace already holds data, since that
    /// means `meta` was lost rather than never written.
    fn initialise_meta(&self) -> Result<()> {
        if !self.meta.is_empty()?
            || !self.data.is_empty()?
            || !self.log.is_empty()?
            || !self.replica_log.is_empty()?
            || !self.replica_snapshot.is_empty()?
        {
            return Err(Error::CorruptMeta(
                "format_version missing from a populated database",
            ));
        }
        let mut batch = self.db.batch().durability(Some(PersistMode::SyncAll));
        batch.insert(
            &self.meta,
            FORMAT_VERSION_KEY,
            encode_format_version(FORMAT_VERSION),
        );
        batch.insert(
            &self.meta,
            LAST_COMMIT_TS_KEY,
            encode_timestamp(Timestamp::ZERO),
        );
        batch.insert(
            &self.meta,
            GC_WATERMARK_KEY,
            encode_timestamp(Timestamp::ZERO),
        );
        batch.insert(
            &self.meta,
            LAST_APPLIED_INDEX_KEY,
            encode_timestamp(Timestamp::ZERO),
        );
        batch.commit()?;
        Ok(())
    }

    /// Read a required `meta` entry, failing with `missing` if it is absent.
    fn read_meta(&self, key: &[u8], missing: &'static str) -> Result<fjall::Slice> {
        self.meta.get(key)?.ok_or(Error::CorruptMeta(missing))
    }
}

/// Return the corruption error for a malformed snapshot.
fn malformed_snapshot() -> Error {
    Error::CorruptMeta("malformed snapshot")
}

/// Reads an exported snapshot's fields in order.
struct SnapshotReader<'a>(&'a [u8]);

impl<'a> SnapshotReader<'a> {
    /// Take the next `n` bytes.
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(malformed_snapshot());
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    /// Take a big-endian `u64`.
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    /// Take a big-endian `u32` as a length or count.
    fn u32(&mut self) -> Result<usize> {
        let value = u32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes"));
        usize::try_from(value).map_err(|_| malformed_snapshot())
    }

    /// Take length-prefixed bytes.
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()?;
        self.take(len)
    }
}

/// Decode a log key: a big-endian `u64` index.
fn log_index(key: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = key
        .try_into()
        .map_err(|_| Error::CorruptMeta("log key is not 8 bytes"))?;
    Ok(u64::from_be_bytes(bytes))
}

impl std::fmt::Debug for Engine {
    /// Render the engine without exposing fjall internals.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a raw fjall database at `path` and write `key = value` into `meta`.
    fn write_raw_meta(path: &Path, key: &[u8], value: &[u8]) {
        let db = Database::builder(path).open().unwrap();
        let meta = db
            .keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)
            .unwrap();
        meta.insert(key, value).unwrap();
        db.persist(PersistMode::SyncAll).unwrap();
    }

    #[test]
    /// Assert a fresh directory is initialised and the values survive reopening.
    fn fresh_database_initialises_meta() {
        let dir = tempfile::tempdir().unwrap();
        {
            let engine = Engine::open(dir.path()).unwrap();
            assert_eq!(engine.format_version().unwrap(), FORMAT_VERSION);
            assert_eq!(engine.last_commit_ts().unwrap(), Timestamp::ZERO);
            assert_eq!(engine.gc_watermark().unwrap(), Timestamp::ZERO);
            engine.data().insert(b"k", b"v").unwrap();
            engine.database().persist(PersistMode::SyncAll).unwrap();
        }
        let engine = Engine::open(dir.path()).unwrap();
        assert_eq!(engine.format_version().unwrap(), FORMAT_VERSION);
        assert_eq!(engine.data().get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
    }

    /// Return a log entry with one transaction per `(key, value)` pair.
    fn entry(index: u64, first_ts: u64, writes: &[(&str, Option<&str>)]) -> LogEntry {
        use crate::commit::LogWrite;
        LogEntry {
            index,
            first_ts: Timestamp::from_raw(first_ts),
            txns: writes
                .iter()
                .map(|(k, v)| {
                    vec![LogWrite {
                        key: bytes::Bytes::copy_from_slice(k.as_bytes()),
                        value: v.map(|v| bytes::Bytes::copy_from_slice(v.as_bytes())),
                    }]
                })
                .collect(),
        }
    }

    /// Return the log indexes present.
    fn logged(engine: &Engine) -> Vec<u64> {
        engine
            .log
            .iter()
            .map(|item| u64::from_be_bytes(item.key().unwrap()[..].try_into().unwrap()))
            .collect()
    }

    #[test]
    /// Assert entries appended to the log but not applied before a crash
    /// are applied when the database opens, in order, and only once.
    fn unapplied_entries_are_replayed() {
        let dir = tempfile::tempdir().unwrap();
        {
            let engine = Engine::open(dir.path()).unwrap();
            engine
                .append_log(&entry(1, 1, &[("a", Some("1"))]), Durability::Sync)
                .unwrap();
            engine
                .apply_entry(&entry(1, 1, &[("a", Some("1"))]))
                .unwrap();
            // Committed to the log; the process stops before applying them.
            engine
                .append_log(
                    &entry(2, 2, &[("b", Some("2")), ("a", None)]),
                    Durability::Sync,
                )
                .unwrap();
            engine
                .append_log(&entry(3, 4, &[("c", Some("3"))]), Durability::Sync)
                .unwrap();
            assert_eq!(engine.last_applied_index().unwrap(), 1);
        }
        let engine = Engine::open(dir.path()).unwrap();
        assert_eq!(engine.replay_log().unwrap(), (Timestamp::from_raw(4), 3));
        assert_eq!(engine.last_applied_index().unwrap(), 3);
        for (key, ts, value) in [
            ("b", 2, Some(&b"2"[..])),
            ("a", 3, None),
            ("c", 4, Some(b"3")),
        ] {
            let stored = engine
                .data()
                .get(versioned_key(key.as_bytes(), Timestamp::from_raw(ts)))
                .unwrap()
                .expect("replayed version");
            assert_eq!(stored, encode_stored(value), "{key}@{ts}");
        }
        // Replaying again changes nothing.
        assert_eq!(engine.replay_log().unwrap(), (Timestamp::from_raw(4), 3));
    }

    #[test]
    /// Assert compaction removes applied entries up to the index given and
    /// keeps the rest, and a later open neither reapplies nor reuses them.
    fn compaction_keeps_unapplied_entries() {
        let dir = tempfile::tempdir().unwrap();
        {
            let engine = Engine::open(dir.path()).unwrap();
            for i in 1..=5 {
                let e = entry(i, i, &[("k", Some("v"))]);
                engine.append_log(&e, Durability::Sync).unwrap();
                engine.apply_entry(&e).unwrap();
            }
            engine.compact_log(3).unwrap();
            assert_eq!(logged(&engine), [4, 5]);
            engine.compact_log(5).unwrap();
            assert!(logged(&engine).is_empty());
        }
        let engine = Engine::open(dir.path()).unwrap();
        // Applied index 5 is kept in `meta`, so new entries continue after it.
        assert_eq!(engine.replay_log().unwrap(), (Timestamp::from_raw(5), 5));
    }

    #[test]
    /// Assert a gap in the log is reported as corruption, not skipped.
    fn gaps_are_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        engine
            .append_log(&entry(2, 1, &[("k", Some("v"))]), Durability::Sync)
            .unwrap();
        assert!(matches!(
            engine.replay_log(),
            Err(Error::CorruptMeta("gap in the commit log"))
        ));
    }

    #[test]
    /// Assert a database from another format version is refused.
    fn unsupported_format_version() {
        let dir = tempfile::tempdir().unwrap();
        drop(Engine::open(dir.path()).unwrap());
        write_raw_meta(dir.path(), FORMAT_VERSION_KEY, &encode_format_version(99));
        let err = Engine::open(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            Error::UnsupportedFormatVersion {
                found: 99,
                supported: FORMAT_VERSION
            }
        ));
    }

    #[test]
    /// Assert a populated database without a format version is not silently re-initialised.
    fn missing_format_version_on_populated_database() {
        let dir = tempfile::tempdir().unwrap();
        write_raw_meta(
            dir.path(),
            LAST_COMMIT_TS_KEY,
            &encode_timestamp(Timestamp::from_raw(5)),
        );
        let err = Engine::open(dir.path()).unwrap_err();
        assert!(matches!(err, Error::CorruptMeta(_)));
    }

    #[test]
    /// Assert malformed `meta` values are reported as corruption.
    fn malformed_meta_value() {
        let dir = tempfile::tempdir().unwrap();
        drop(Engine::open(dir.path()).unwrap());
        write_raw_meta(dir.path(), GC_WATERMARK_KEY, b"short");
        let err = Engine::open(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            Error::CorruptMeta("gc_watermark is not 8 bytes")
        ));
    }

    #[test]
    /// Assert a snapshot carries only current data: the newest version of
    /// each key, at its commit timestamp, without deleted keys, and the
    /// receiver's GC watermark rises to the snapshot's last commit.
    fn snapshots_carry_current_data_only() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        let ts = Timestamp::from_raw;
        /// A version to write: its timestamp, key and value, or `None` to delete.
        type Version<'a> = (Timestamp, &'a [u8], Option<&'a [u8]>);
        let writes: [Version<'_>; 5] = [
            (ts(1), b"k", Some(b"old")),
            (ts(2), b"k", Some(b"new")),
            (ts(3), b"gone", Some(b"x")),
            (ts(4), b"gone", None),
            (ts(5), b"e", Some(b"y")),
        ];
        for (at, key, value) in writes {
            engine
                .apply_versions([(at, key, value)], Some(at), &[])
                .unwrap();
        }
        engine
            .set_meta(&[(b"raft_applied".to_vec(), b"5".to_vec())], false)
            .unwrap();
        let snapshot = engine
            .export(&engine.view(), &[b"raft_applied".to_vec()])
            .unwrap();

        let other_dir = tempfile::tempdir().unwrap();
        let other = Engine::open(other_dir.path()).unwrap();
        other
            .apply_versions([(ts(1), &b"stale"[..], Some(&b"z"[..]))], Some(ts(1)), &[])
            .unwrap();
        assert_eq!(other.import(&snapshot).unwrap(), ts(5));
        let versions: Vec<(Vec<u8>, u64, Vec<u8>)> = other
            .data()
            .iter()
            .map(|item| {
                let (key, value) = item.into_inner().unwrap();
                let (user, at) = split_versioned_key(&key).unwrap();
                (user.to_vec(), at.into_raw(), value.to_vec())
            })
            .collect();
        assert_eq!(
            versions,
            vec![
                (b"e".to_vec(), 5, encode_stored(Some(b"y"))),
                (b"k".to_vec(), 2, encode_stored(Some(b"new"))),
            ]
        );
        assert_eq!(other.last_commit_ts().unwrap(), ts(5));
        assert_eq!(other.gc_watermark().unwrap(), ts(5));
        assert_eq!(
            other.get_meta(b"raft_applied").unwrap(),
            Some(b"5".to_vec())
        );
    }
}
