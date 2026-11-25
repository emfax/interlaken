//! Opening the fjall database and its keyspaces.

use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};

use bytes::Bytes;
use fjall::{Database, Keyspace, KeyspaceCreateOptions, KvSeparationOptions, PersistMode};

use super::meta::{
    FORMAT_VERSION, FORMAT_VERSION_KEY, GC_WATERMARK_KEY, LAST_APPLIED_INDEX_KEY,
    LAST_COMMIT_TS_KEY, decode_format_version, decode_timestamp, encode_format_version,
    encode_timestamp,
};
use super::snapshot::{self, Decoder, Event, ExportCursor};
use super::stored::{StoredValue, decode_stored, encode_stored};
use crate::changes::{ChangeEntry, durable_key};
use crate::commit::LogEntry;
use crate::encoding::{split_versioned_key, versioned_key};
use crate::telemetry;
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
/// The keyspace of the durable change log (ADR-0040).
const CHANGES_KEYSPACE: &str = "changes";
/// The `meta` key of the durable change log's floor (ADR-0040).
pub(super) const CHANGES_FLOOR_KEY: &[u8] = b"changes_floor";
/// Name of the keyspace that held stored snapshots before data generations
/// (ADR-0037 phase 4); removed on open.
const OLD_SNAPSHOT_KEYSPACE: &str = "replica_snapshot";
/// The `meta` key of the current data keyspace generation (ADR-0038).
const DATA_GENERATION_KEY: &[u8] = b"data_generation";
/// Memtable bytes past which bulk snapshot writes wait for flushes.
const WRITE_BUFFER_LIMIT: u64 = 16 * 1024 * 1024;

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
    /// The current generation of the versioned keyspace, switched when a
    /// replica installs a snapshot (ADR-0038).
    data: Arc<RwLock<DataKeyspace>>,
    meta: Keyspace,
    log: Keyspace,
    replica_log: Keyspace,
    /// The durable change log, by commit timestamp (ADR-0040).
    pub(super) changes: Keyspace,
    /// Memtable size for keyspaces created later, such as data generations.
    memtable_bytes: u64,
}

/// fjall's sizes and backlogs at one moment (ADR-0039).
#[derive(Clone, Copy, Debug)]
pub(crate) struct EngineSample {
    /// Bytes on disk in the current data generation.
    pub(crate) data_bytes: u64,
    /// Bytes on disk in journals.
    pub(crate) journal_bytes: u64,
    /// Every other keyspace's bytes on disk.
    pub(crate) other_bytes: u64,
    /// Bytes in memtables, active and awaiting flush.
    pub(crate) memtable_bytes: u64,
    /// Memtable flushes queued.
    pub(crate) flush_backlog: usize,
    /// Tables in level 0 of the data keyspace.
    pub(crate) l0_tables: usize,
    /// Time spent compacting since the database was opened.
    pub(crate) compacting: std::time::Duration,
}

/// The current generation of the versioned keyspace.
struct DataKeyspace {
    /// Its generation: 0 for the original `data` keyspace.
    generation: u64,
    /// The keyspace.
    keyspace: Keyspace,
}

/// Values at least this large are kept apart from the keys in the `data`
/// keyspace, so compactions don't rewrite them: file chunks are up to
/// 1 MiB (ADR-0044).
const SEPARATION_THRESHOLD: u32 = 64 * 1024;

/// Return the options of a `data` keyspace: large values are separated.
/// The options are fixed when a keyspace is created, so a database made
/// before this keeps its layout until a snapshot install makes a new one.
fn data_options(memtable_bytes: u64) -> KeyspaceCreateOptions {
    KeyspaceCreateOptions::default()
        .max_memtable_size(memtable_bytes)
        .with_kv_separation(Some(
            KvSeparationOptions::default().separation_threshold(SEPARATION_THRESHOLD),
        ))
}

/// Return the name of data keyspace generation `generation`: `data` for the
/// first, so databases from before generations open unchanged.
fn data_name(generation: u64) -> String {
    if generation == 0 {
        DATA_KEYSPACE.to_owned()
    } else {
        format!("{DATA_KEYSPACE}.{generation}")
    }
}

/// A snapshot being received into a new data generation (ADR-0038).
pub(crate) struct Staging {
    /// The generation being filled.
    generation: u64,
    /// Its keyspace.
    keyspace: Keyspace,
    /// Decodes and checks the chunks as they arrive.
    decoder: Decoder,
    /// The snapshot's `meta` entries.
    meta: Vec<(Vec<u8>, Vec<u8>)>,
}

/// A snapshot received in full into a new data generation, synced and
/// checked, ready to switch to.
pub(crate) struct Staged {
    /// The generation.
    generation: u64,
    /// Its keyspace.
    pub(crate) keyspace: Keyspace,
    /// The snapshot's `meta` entries.
    meta: Vec<(Vec<u8>, Vec<u8>)>,
    /// The snapshot's last commit timestamp.
    last: Timestamp,
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
        let meta = db.keyspace(META_KEYSPACE, create)?;
        let generation = match meta.get(DATA_GENERATION_KEY)? {
            Some(value) => u64::from_be_bytes(
                value[..]
                    .try_into()
                    .map_err(|_| Error::CorruptMeta("data generation is not 8 bytes"))?,
            ),
            None => 0,
        };
        // A crash may leave the generation an install was filling, or the
        // one an install switched away from: neither is current.
        let stale = [Some(generation + 1), generation.checked_sub(1)];
        for name in stale
            .into_iter()
            .flatten()
            .map(data_name)
            .chain([OLD_SNAPSHOT_KEYSPACE.to_owned()])
        {
            if db.keyspace_exists(&name) {
                let handle = db.keyspace(&name, create)?;
                db.delete_keyspace(handle)?;
            }
        }
        let data = db.keyspace(&data_name(generation), || data_options(memtable_bytes))?;
        let log = db.keyspace(LOG_KEYSPACE, create)?;
        let replica_log = db.keyspace(REPLICA_LOG_KEYSPACE, create)?;
        let changes = db.keyspace(CHANGES_KEYSPACE, create)?;
        let engine = Self {
            db,
            data: Arc::new(RwLock::new(DataKeyspace {
                generation,
                keyspace: data,
            })),
            meta,
            log,
            replica_log,
            changes,
            memtable_bytes,
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

    /// Return the current generation of the versioned `data` keyspace.
    /// The handle is cheap to clone, and stays usable after a switch.
    pub(crate) fn data(&self) -> Keyspace {
        self.data
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keyspace
            .clone()
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
    ///
    /// `changes` are written to the durable change log in the same batch
    /// (ADR-0040); pass none unless the log is durable.
    pub(crate) fn commit_group<'a>(
        &self,
        last: Timestamp,
        writes: impl IntoIterator<Item = (Timestamp, &'a [u8], Option<&'a [u8]>)>,
        changes: &[Arc<ChangeEntry>],
        durability: Durability,
    ) -> Result<()> {
        let mode = match durability {
            Durability::Sync => PersistMode::SyncAll,
            Durability::Buffered => PersistMode::Buffer,
        };
        let data = self.data();
        let mut batch = self.db.batch().durability(Some(mode));
        for (ts, key, value) in writes {
            batch.insert(&data, versioned_key(key, ts), encode_stored(value));
        }
        self.batch_changes(&mut batch, changes);
        batch.insert(&self.meta, LAST_COMMIT_TS_KEY, encode_timestamp(last));
        batch.commit()?;
        Ok(())
    }

    /// Add `changes` to `batch` in the durable change log.
    fn batch_changes(&self, batch: &mut fjall::OwnedWriteBatch, changes: &[Arc<ChangeEntry>]) {
        for entry in changes {
            batch.insert(&self.changes, durable_key(entry.ts), entry.encode());
        }
    }

    /// Add to `batch` the removal of every durable change, and a floor of `last`.
    fn batch_reset_changes(
        &self,
        batch: &mut fjall::OwnedWriteBatch,
        last: Timestamp,
    ) -> Result<()> {
        for item in self.changes.iter() {
            batch.remove(&self.changes, item.key()?);
        }
        batch.insert(&self.meta, CHANGES_FLOOR_KEY, encode_timestamp(last));
        Ok(())
    }

    /// Open the change log keeping `retention`, given the last commit
    /// (ADR-0040).
    ///
    /// A durable log reloads its index from the `changes` keyspace, with a
    /// floor first set to the last commit when the log is new. Any other
    /// log starts empty, and removes durable changes a previous open left,
    /// so a later durable open can't mistake them for a complete history.
    pub(crate) fn open_changes(
        &self,
        retention: crate::changes::ChangeRetention,
        last: Timestamp,
    ) -> Result<crate::changes::ChangeLog> {
        use crate::changes::{ChangeLog, ChangeRetention, decode_durable_key};
        let stored_floor = self
            .meta
            .get(CHANGES_FLOOR_KEY)?
            .map(|v| decode_timestamp(&v, "changes_floor is not 8 bytes"))
            .transpose()?;
        if !matches!(retention, ChangeRetention::Durable { .. }) {
            if stored_floor.is_some() {
                let mut batch = self.db.batch();
                for item in self.changes.iter() {
                    batch.remove(&self.changes, item.key()?);
                }
                batch.remove(&self.meta, CHANGES_FLOOR_KEY);
                batch.commit()?;
            }
            return Ok(ChangeLog::new(retention, last, last, Vec::new()));
        }
        let Some(floor) = stored_floor else {
            let mut batch = self.db.batch().durability(Some(PersistMode::SyncAll));
            batch.insert(&self.meta, CHANGES_FLOOR_KEY, encode_timestamp(last));
            batch.commit()?;
            return Ok(ChangeLog::new(retention, last, last, Vec::new()));
        };
        let mut index = Vec::new();
        for item in self.changes.iter() {
            let (key, value) = item.into_inner()?;
            let ts = decode_durable_key(&key)?;
            let micros = value
                .get(..8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_be_bytes)
                .ok_or(Error::CorruptMeta("malformed change log entry"))?;
            index.push((ts, (key.len() + value.len()) as u64, micros));
        }
        Ok(ChangeLog::new(retention, last, floor, index))
    }

    /// Return up to `max` durable change entries after `after` and at or
    /// before `upto`, in commit order (ADR-0040).
    pub(crate) fn read_changes(
        &self,
        after: Timestamp,
        upto: Timestamp,
        max: usize,
    ) -> Result<Vec<Arc<ChangeEntry>>> {
        let Some(start) = after.into_raw().checked_add(1) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for item in self
            .changes
            .range(start.to_be_bytes()..=upto.into_raw().to_be_bytes())
            .take(max)
        {
            let (key, value) = item.into_inner()?;
            let ts = crate::changes::decode_durable_key(&key)?;
            out.push(Arc::new(ChangeEntry::decode(ts, &value)?));
        }
        Ok(out)
    }

    /// Remove the durable change entries up to and including `through`,
    /// and raise the floor to it, in one batch (ADR-0040).
    pub(crate) fn trim_changes(&self, through: Timestamp) -> Result<()> {
        let mut batch = self.db.batch();
        for item in self.changes.range(..=through.into_raw().to_be_bytes()) {
            batch.remove(&self.changes, item.key()?);
        }
        batch.insert(&self.meta, CHANGES_FLOOR_KEY, encode_timestamp(through));
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
    /// replaying it on open restores anything a crash loses. `changes` are
    /// written to the durable change log in the same batch (ADR-0040).
    pub(crate) fn apply_entry(&self, entry: &LogEntry, changes: &[Arc<ChangeEntry>]) -> Result<()> {
        let data = self.data();
        let mut batch = self.db.batch();
        self.batch_changes(&mut batch, changes);
        for (ts, write) in entry.versions() {
            batch.insert(
                &data,
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
    ///
    /// With `durable_changes`, each entry's changes are written to the
    /// durable change log as it is applied (ADR-0040).
    pub(crate) fn replay_log(&self, durable_changes: bool) -> Result<(Timestamp, u64)> {
        let applied = self.last_applied_index()?;
        let mut last_index = applied;
        for item in self.log.range((applied + 1).to_be_bytes()..) {
            let entry = LogEntry::decode(&item.value()?)?;
            if entry.index != last_index + 1 {
                return Err(Error::CorruptMeta("gap in the commit log"));
            }
            let changes = if durable_changes {
                entry.changes()
            } else {
                Vec::new()
            };
            self.apply_entry(&entry, &changes)?;
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
    ///
    /// `changes` are written to the durable change log in the same batch
    /// (ADR-0040).
    pub(crate) fn apply_versions<'a>(
        &self,
        writes: impl IntoIterator<Item = (Timestamp, &'a [u8], Option<&'a [u8]>)>,
        last: Option<Timestamp>,
        meta: &[(Vec<u8>, Vec<u8>)],
        changes: &[Arc<ChangeEntry>],
    ) -> Result<()> {
        let data = self.data();
        let mut batch = self.db.batch();
        self.batch_changes(&mut batch, changes);
        for (ts, key, value) in writes {
            batch.insert(&data, versioned_key(key, ts), encode_stored(value));
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

    /// Return a consistent view of every keyspace, with the data keyspace
    /// it is a view of, for exporting a snapshot.
    pub(crate) fn view(&self) -> (fjall::Snapshot, Keyspace) {
        // Read the data generation first: a switch after it leaves the view
        // seeing the old generation as it was, which is still consistent.
        let data = self.data();
        (self.db.snapshot(), data)
    }

    /// Encode the next chunk of the current data in `view` (ADR-0038; see
    /// [`snapshot`](super::snapshot) for the format), resuming at `cursor`:
    /// the newest version of each key at or below the view's last commit,
    /// leaving out keys whose newest version is a deletion. A chunk ends
    /// after the key that takes it past `budget` bytes. Return the chunk and
    /// where the next one resumes, or `None` after the last chunk, which
    /// holds the trailer.
    pub(crate) fn export_chunk(
        &self,
        view: &fjall::Snapshot,
        data: &Keyspace,
        meta_keys: &[Vec<u8>],
        mut cursor: ExportCursor,
        budget: usize,
    ) -> Result<(Vec<u8>, Option<ExportCursor>)> {
        use fjall::Readable as _;
        let last = self
            .view_meta(view, LAST_COMMIT_TS_KEY)?
            .as_deref()
            .map(|v| decode_timestamp(v, "snapshot timestamp"))
            .transpose()?
            .ok_or(Error::CorruptMeta("snapshot view lacks a timestamp"))?;
        let mut out = Vec::new();
        if !cursor.started {
            out.extend_from_slice(snapshot::MAGIC);
            out.extend_from_slice(&last.into_raw().to_be_bytes());
            let mut present = Vec::new();
            for key in meta_keys {
                if let Some(value) = self.view_meta(view, key)? {
                    present.push((key, value));
                }
            }
            out.extend_from_slice(
                &u32::try_from(present.len())
                    .expect("meta count fits u32")
                    .to_be_bytes(),
            );
            for (key, value) in &present {
                snapshot::put_bytes(&mut out, key);
                snapshot::put_bytes(&mut out, value);
            }
            cursor.started = true;
        }
        // Versions of a key sort together, newest first, and no key is a
        // prefix of another: resuming after a key's oldest possible version
        // skips the rest of its versions.
        let start = match &cursor.after {
            Some(key) => std::ops::Bound::Excluded(versioned_key(key, Timestamp::ZERO)),
            None => std::ops::Bound::Unbounded,
        };
        let mut decided: Option<Vec<u8>> = None;
        for item in view.range(data, (start, std::ops::Bound::Unbounded)) {
            let (versioned, value) = item.into_inner()?;
            let (key, ts) = split_versioned_key(&versioned)?;
            if decided.as_deref() == Some(key) || ts > last {
                continue;
            }
            if out.len() >= budget && decided.is_some() {
                cursor.after = decided;
                return Ok((out, Some(cursor)));
            }
            decided = Some(key.to_vec());
            if let StoredValue::Put(value) = decode_stored(Bytes::copy_from_slice(&value))? {
                snapshot::put_bytes(&mut out, key);
                out.extend_from_slice(&ts.into_raw().to_be_bytes());
                snapshot::put_bytes(&mut out, &value);
                cursor.entries += 1;
            }
        }
        out.extend_from_slice(snapshot::END);
        out.extend_from_slice(&cursor.entries.to_be_bytes());
        Ok((out, None))
    }

    /// Encode all of the current data in `view` as one snapshot.
    pub(crate) fn export(
        &self,
        view: &fjall::Snapshot,
        data: &Keyspace,
        meta_keys: &[Vec<u8>],
    ) -> Result<Vec<u8>> {
        let (out, rest) =
            self.export_chunk(view, data, meta_keys, ExportCursor::default(), usize::MAX)?;
        debug_assert!(rest.is_none(), "an unlimited chunk holds everything");
        Ok(out)
    }

    /// Wait, for at most two seconds, while memtables hold more than
    /// [`WRITE_BUFFER_LIMIT`]. fjall doesn't bound memtables awaiting flush,
    /// so a bulk write, such as a snapshot, could otherwise queue up to its
    /// whole size in memory. Runs on a storage thread, which it blocks.
    fn throttle_writes(&self) {
        // Bounded, so a stalled flush delays the write rather than hangs it.
        let started = std::time::Instant::now();
        let deadline = started + std::time::Duration::from_secs(2);
        let mut waited = false;
        while self.db.write_buffer_size() > WRITE_BUFFER_LIMIT
            && std::time::Instant::now() < deadline
        {
            waited = true;
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if waited {
            let wait = started.elapsed();
            metrics::gauge!(telemetry::WRITE_THROTTLE).increment(telemetry::seconds(wait));
            if wait >= std::time::Duration::from_secs(2) {
                tracing::warn!(
                    memtable_bytes = self.db.write_buffer_size(),
                    "memtables still above the write limit after the longest wait",
                );
            }
        }
    }

    /// Begin receiving a snapshot into a new data generation, removing any
    /// generation an unfinished install left (ADR-0038).
    pub(crate) fn begin_staging(&self) -> Result<Staging> {
        let generation = self
            .data
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .generation
            + 1;
        let name = data_name(generation);
        let create = || data_options(self.memtable_bytes);
        if self.db.keyspace_exists(&name) {
            let stale = self.db.keyspace(&name, create)?;
            self.db.delete_keyspace(stale)?;
        }
        Ok(Staging {
            generation,
            keyspace: self.db.keyspace(&name, create)?,
            decoder: Decoder::new(),
            meta: Vec::new(),
        })
    }

    /// Decode the next chunk of a snapshot being received, checking it, and
    /// write its versions into the new generation at their original commit
    /// timestamps, unsynced.
    pub(crate) fn stage_chunk(&self, staging: &mut Staging, chunk: &[u8]) -> Result<()> {
        let Staging {
            keyspace,
            decoder,
            meta,
            ..
        } = staging;
        let mut batch = self.db.batch();
        decoder.feed(chunk, &mut |event| {
            match event {
                Event::Meta(key, value) => meta.push((key.to_vec(), value.to_vec())),
                Event::Entry(key, ts, value) => {
                    batch.insert(keyspace, versioned_key(key, ts), encode_stored(Some(value)));
                }
            }
            Ok(())
        })?;
        batch.commit()?;
        self.throttle_writes();
        Ok(())
    }

    /// Finish receiving a snapshot: check it ended properly, and sync its
    /// generation.
    pub(crate) fn finish_staging(&self, staging: Staging) -> Result<Staged> {
        let last = staging.decoder.finish()?;
        self.persist()?;
        Ok(Staged {
            generation: staging.generation,
            keyspace: staging.keyspace,
            meta: staging.meta,
            last,
        })
    }

    /// Remove a data generation that will never be switched to.
    pub(crate) fn discard_staging(&self, keyspace: Keyspace) -> Result<()> {
        self.db.delete_keyspace(keyspace)?;
        Ok(())
    }

    /// Switch to a received snapshot's generation, and return its last
    /// commit timestamp.
    ///
    /// One synced batch makes the generation current and records the
    /// snapshot's last commit timestamp, its `meta` entries, and a GC
    /// watermark of at least its last commit, since no older versions
    /// exist in it. The old generation is then deleted; readers still using
    /// it keep it until they finish. A crash before the batch leaves the old
    /// generation current, and the new one is removed on open.
    pub(crate) fn switch_to(&self, staged: Staged) -> Result<Timestamp> {
        let watermark = self.gc_watermark()?.max(staged.last);
        let mut batch = self.db.batch().durability(Some(PersistMode::SyncAll));
        batch.insert(
            &self.meta,
            DATA_GENERATION_KEY,
            staged.generation.to_be_bytes(),
        );
        batch.insert(
            &self.meta,
            LAST_COMMIT_TS_KEY,
            encode_timestamp(staged.last),
        );
        batch.insert(&self.meta, GC_WATERMARK_KEY, encode_timestamp(watermark));
        for (key, value) in &staged.meta {
            batch.insert(&self.meta, key.as_slice(), value.as_slice());
        }
        // The commits the snapshot covers were never applied here, so the
        // durable change log restarts at its last commit (ADR-0040).
        self.batch_reset_changes(&mut batch, staged.last)?;
        batch.commit()?;
        let old = std::mem::replace(
            &mut *self.data.write().unwrap_or_else(PoisonError::into_inner),
            DataKeyspace {
                generation: staged.generation,
                keyspace: staged.keyspace,
            },
        );
        self.db.delete_keyspace(old.keyspace)?;
        Ok(staged.last)
    }

    /// Install a snapshot held whole in memory: receive it in one chunk and
    /// switch to it. A malformed snapshot leaves the data as it was.
    pub(crate) fn import(&self, snapshot: &[u8]) -> Result<Timestamp> {
        let mut staging = self.begin_staging()?;
        let staged = self
            .stage_chunk(&mut staging, snapshot)
            .and_then(|()| self.finish_staging(staging));
        match staged {
            Ok(staged) => self.switch_to(staged),
            Err(error) => {
                let name = data_name(
                    self.data
                        .read()
                        .unwrap_or_else(PoisonError::into_inner)
                        .generation
                        + 1,
                );
                let create =
                    || KeyspaceCreateOptions::default().max_memtable_size(self.memtable_bytes);
                self.discard_staging(self.db.keyspace(&name, create)?)?;
                Err(error)
            }
        }
    }

    /// Return the `meta` entry at `key` as `view` sees it.
    pub(crate) fn view_meta(&self, view: &fjall::Snapshot, key: &[u8]) -> Result<Option<Vec<u8>>> {
        use fjall::Readable as _;
        Ok(view.get(&self.meta, key)?.map(|v| v.to_vec()))
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
        let data = self.data();
        let mut batch = self.db.batch();
        for key in keys {
            batch.remove(&data, key);
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

    /// Sample fjall's sizes and backlogs for the metrics sampler (ADR-0039).
    pub(crate) fn sample(&self) -> Result<EngineSample> {
        let total = self.db.disk_space()?;
        let journal = self.db.journal_disk_space()?;
        let data = self.data();
        let data_bytes = data.disk_space();
        Ok(EngineSample {
            data_bytes,
            journal_bytes: journal,
            other_bytes: total.saturating_sub(journal).saturating_sub(data_bytes),
            memtable_bytes: self.db.write_buffer_size(),
            flush_backlog: self.db.outstanding_flushes(),
            l0_tables: data.l0_table_count(),
            compacting: self.db.time_compacting(),
        })
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
            || !self.data().is_empty()?
            || !self.log.is_empty()?
            || !self.replica_log.is_empty()?
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
                        prior: crate::commit::Prior::Unknown,
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
                .apply_entry(&entry(1, 1, &[("a", Some("1"))]), &[])
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
        assert_eq!(
            engine.replay_log(false).unwrap(),
            (Timestamp::from_raw(4), 3)
        );
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
        assert_eq!(
            engine.replay_log(false).unwrap(),
            (Timestamp::from_raw(4), 3)
        );
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
                engine.apply_entry(&e, &[]).unwrap();
            }
            engine.compact_log(3).unwrap();
            assert_eq!(logged(&engine), [4, 5]);
            engine.compact_log(5).unwrap();
            assert!(logged(&engine).is_empty());
        }
        let engine = Engine::open(dir.path()).unwrap();
        // Applied index 5 is kept in `meta`, so new entries continue after it.
        assert_eq!(
            engine.replay_log(false).unwrap(),
            (Timestamp::from_raw(5), 5)
        );
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
            engine.replay_log(false),
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
                .apply_versions([(at, key, value)], Some(at), &[], &[])
                .unwrap();
        }
        engine
            .set_meta(&[(b"raft_applied".to_vec(), b"5".to_vec())], false)
            .unwrap();
        let (view, data) = engine.view();
        let snapshot = engine
            .export(&view, &data, &[b"raft_applied".to_vec()])
            .unwrap();

        let other_dir = tempfile::tempdir().unwrap();
        let other = Engine::open(other_dir.path()).unwrap();
        other
            .apply_versions(
                [(ts(1), &b"stale"[..], Some(&b"z"[..]))],
                Some(ts(1)),
                &[],
                &[],
            )
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
