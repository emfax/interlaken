//! Opening the fjall database and its keyspaces.

use std::path::Path;

use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};

use super::meta::{
    FORMAT_VERSION, FORMAT_VERSION_KEY, GC_WATERMARK_KEY, LAST_COMMIT_TS_KEY,
    decode_format_version, decode_timestamp, encode_format_version, encode_timestamp,
};
use super::stored::encode_stored;
use crate::encoding::versioned_key;
use crate::{Durability, Error, Result, Timestamp};

/// Name of the versioned keyspace holding rows, index entries and the catalog.
const DATA_KEYSPACE: &str = "data";
/// Name of the unversioned keyspace holding engine state.
const META_KEYSPACE: &str = "meta";

/// The fjall database with its `data` and `meta` keyspaces.
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
        let db = Database::builder(path.as_ref()).open()?;
        let data = db.keyspace(DATA_KEYSPACE, KeyspaceCreateOptions::default)?;
        let meta = db.keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)?;
        let engine = Self { db, data, meta };

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
    /// journal is synced with `SyncAll` before this returns.
    pub(crate) fn commit_group<'a>(
        &self,
        last: Timestamp,
        writes: impl IntoIterator<Item = (Timestamp, &'a [u8], Option<&'a [u8]>)>,
        durability: Durability,
    ) -> Result<()> {
        let mode = match durability {
            Durability::Sync => Some(PersistMode::SyncAll),
            Durability::Buffered => None,
        };
        let mut batch = self.db.batch().durability(mode);
        for (ts, key, value) in writes {
            batch.insert(&self.data, versioned_key(key, ts), encode_stored(value));
        }
        batch.insert(&self.meta, LAST_COMMIT_TS_KEY, encode_timestamp(last));
        batch.commit()?;
        Ok(())
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
        if !self.meta.is_empty()? || !self.data.is_empty()? {
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
        batch.commit()?;
        Ok(())
    }

    /// Read a required `meta` entry, failing with `missing` if it is absent.
    fn read_meta(&self, key: &[u8], missing: &'static str) -> Result<fjall::Slice> {
        self.meta.get(key)?.ok_or(Error::CorruptMeta(missing))
    }
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
}
