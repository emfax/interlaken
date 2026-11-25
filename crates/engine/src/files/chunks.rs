//! File content in chunks addressed by their BLAKE3 hash (ADR-0044).
//!
//! Every chunk has a row in the engine table `engine_fs_chunks`, keyed by
//! namespace and hash, which clients can't query: a chunk is reached only
//! through a file whose metadata the reader may see. The row holds the
//! content itself, or, for a namespace whose content is in an object
//! store, the store and the key of the object holding it.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use interlaken_storage::catalog::{self, ColumnDef, TableDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::{FjallStorage, Storage, Txn, TxnOptions, rows};

use super::stores::{FileStores, object_key};
use crate::EngineError;

/// The table of chunks.
pub(crate) const CHUNKS: &str = "engine_fs_chunks";
/// Objects no chunk refers to any more, waiting to be deleted.
pub(crate) const TRASH: &str = "engine_fs_trash";

/// The size of every chunk but a file's last.
pub const CHUNK_SIZE: usize = 1 << 20;

/// How many times staging a chunk retries after a conflicting commit.
const STAGE_ATTEMPTS: usize = 5;

/// The columns of a chunk row, in order.
mod column {
    /// The content, for a chunk in the database.
    pub(super) const BYTES: usize = 2;
    /// The object store, for a chunk in one.
    pub(super) const STORE: usize = 3;
    /// The object's key.
    pub(super) const OBJECT: usize = 4;
    /// The content's length.
    pub(super) const SIZE: usize = 5;
    /// When it was last staged, in microseconds since the epoch.
    pub(super) const WRITTEN: usize = 6;
}

/// Return the BLAKE3 hash of `bytes`, as lowercase hex.
pub(crate) fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Return the current time in microseconds since the Unix epoch.
pub(crate) fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

/// Return the chunks table, creating it if this database has none yet.
pub(crate) async fn chunks_table(txn: &Txn) -> Result<TableDef, EngineError> {
    if let Some(table) = catalog::table(txn, CHUNKS).await? {
        return Ok(table);
    }
    let not_null = |name: &str, ty| ColumnDef::new(name, ty).not_null();
    Ok(catalog::create_table(
        txn,
        TableSpec {
            name: CHUNKS.into(),
            columns: vec![
                not_null("namespace", ColumnType::Text),
                not_null("hash", ColumnType::Text),
                ColumnDef::new("bytes", ColumnType::Bytea),
                ColumnDef::new("store", ColumnType::Text),
                ColumnDef::new("object", ColumnType::Text),
                not_null("size", ColumnType::Int8),
                not_null("written", ColumnType::Int8),
            ],
            primary_key: vec!["namespace".into(), "hash".into()],
        },
    )
    .await?)
}

/// Return the trash table, creating it if this database has none yet.
pub(crate) async fn trash_table(txn: &Txn) -> Result<TableDef, EngineError> {
    if let Some(table) = catalog::table(txn, TRASH).await? {
        return Ok(table);
    }
    Ok(catalog::create_table(
        txn,
        TableSpec {
            name: TRASH.into(),
            columns: vec![
                ColumnDef::new("store", ColumnType::Text).not_null(),
                ColumnDef::new("object", ColumnType::Text).not_null(),
            ],
            primary_key: vec!["store".into(), "object".into()],
        },
    )
    .await?)
}

/// Return the key of chunk `hash` in `namespace`.
fn key(namespace: &str, hash: &str) -> [Datum; 2] {
    [
        Datum::Text(namespace.to_owned()),
        Datum::Text(hash.to_owned()),
    ]
}

/// Return the text at `i` of `row`, if any.
fn text_at(row: &[Datum], i: usize) -> Option<&str> {
    match row.get(i) {
        Some(Datum::Text(t)) => Some(t),
        _ => None,
    }
}

/// Move the object chunk `row` refers to, if any, to the trash, in `txn`.
pub(crate) async fn trash_object(txn: &Txn, row: &[Datum]) -> Result<(), EngineError> {
    if let (Some(store), Some(object)) = (text_at(row, column::STORE), text_at(row, column::OBJECT))
    {
        let trash = trash_table(txn).await?;
        let entry = vec![
            Datum::Text(store.to_owned()),
            Datum::Text(object.to_owned()),
        ];
        if rows::get(txn, &trash, &entry).await?.is_none() {
            rows::insert(txn, &trash, entry).await?;
        }
    }
    Ok(())
}

/// Return the length of the content of chunk `row`.
pub(crate) fn size_of(row: &[Datum]) -> u64 {
    match row.get(column::SIZE) {
        Some(Datum::Int8(n)) => u64::try_from(*n).unwrap_or(0),
        _ => 0,
    }
}

/// Return when chunk `row` was last staged.
pub(crate) fn written_of(row: &[Datum]) -> i64 {
    match row.get(column::WRITTEN) {
        Some(Datum::Int8(n)) => *n,
        _ => 0,
    }
}

/// Where a namespace's chunks go, and where any chunk is read from.
#[derive(Clone)]
pub(crate) struct ChunkStore {
    /// The database.
    pub(crate) storage: FjallStorage,
    /// The object stores this node can reach.
    pub(crate) stores: Arc<FileStores>,
    /// The namespace.
    pub(crate) namespace: String,
    /// The object store new chunks go to, and the prefix of their keys;
    /// `None` for the database.
    pub(crate) object: Option<(String, String)>,
}

/// What staging found or did in one transaction.
enum Staged {
    /// The chunk is stored, and its row committed.
    Done,
    /// The chunk isn't stored yet: upload it, then record it.
    Upload,
}

impl ChunkStore {
    /// Refresh the row of chunk `digest` if it exists, so collection keeps
    /// it; otherwise insert `row` if given. Commit, and say which happened.
    async fn record(&self, digest: &str, row: Option<Vec<Datum>>) -> Result<Staged, EngineError> {
        let txn = self.storage.begin(TxnOptions::default()).await?;
        let table = chunks_table(&txn).await?;
        let key = key(&self.namespace, digest);
        if let Some(mut existing) = rows::get(&txn, &table, &key).await? {
            existing[column::WRITTEN] = Datum::Int8(now_micros());
            rows::update(&txn, &table, &key, existing).await?;
        } else if let Some(row) = row {
            rows::insert(&txn, &table, row).await?;
        } else {
            return Ok(Staged::Upload);
        }
        txn.commit().await?;
        Ok(Staged::Done)
    }

    /// Return a new row for chunk `digest`.
    fn row(
        &self,
        digest: &str,
        size: usize,
        content: Datum,
        object: Option<(&str, &str)>,
    ) -> Vec<Datum> {
        let text = |t: Option<&str>| t.map_or(Datum::Null, |t| Datum::Text(t.to_owned()));
        vec![
            Datum::Text(self.namespace.clone()),
            Datum::Text(digest.to_owned()),
            content,
            text(object.map(|(store, _)| store)),
            text(object.map(|(_, key)| key)),
            Datum::Int8(i64::try_from(size).unwrap_or(i64::MAX)),
            Datum::Int8(now_micros()),
        ]
    }

    /// Store `bytes` as a chunk, unless an equal chunk is stored already,
    /// which is then refreshed so collection keeps it. Each step commits a
    /// short transaction of its own. Return the chunk's hash.
    ///
    /// For an object store, the object is uploaded before its row
    /// commits, under a key of its own (see [`object_key`]), so a row
    /// never refers to an object that isn't there.
    ///
    /// # Errors
    ///
    /// Returns a storage or object store error, or the last serialization
    /// failure if every attempt conflicted.
    pub(crate) async fn stage(&self, bytes: Vec<u8>) -> Result<String, EngineError> {
        let digest = hash(&bytes);
        let mut last = None;
        for _ in 0..STAGE_ATTEMPTS {
            let attempt = match &self.object {
                None => {
                    let row = self.row(&digest, bytes.len(), Datum::Bytea(bytes.clone()), None);
                    self.record(&digest, Some(row)).await.map(|_| ())
                }
                Some((store, prefix)) => self.stage_object(&digest, &bytes, store, prefix).await,
            };
            match attempt {
                Ok(()) => return Ok(digest),
                Err(e) if e.sqlstate() == "40001" => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| EngineError::Catalog("staging a chunk never ran".into())))
    }

    /// Stage chunk `digest` in object store `store`, under `prefix`.
    async fn stage_object(
        &self,
        digest: &str,
        bytes: &[u8],
        store: &str,
        prefix: &str,
    ) -> Result<(), EngineError> {
        if let Staged::Done = self.record(digest, None).await? {
            return Ok(());
        }
        let object = object_key(prefix, digest);
        self.stores.put(store, &object, bytes.to_vec()).await?;
        let row = self.row(digest, bytes.len(), Datum::Null, Some((store, &object)));
        let recorded = self.record(digest, Some(row)).await;
        let ours = match &recorded {
            // Our row went in unless another staging's was there first.
            Ok(Staged::Done) => self.refers_to(digest, &object).await.unwrap_or(true),
            _ => false,
        };
        if !ours {
            // Nothing refers to our upload; removing it is only tidying.
            let _ = self.stores.delete(store, &object).await;
        }
        recorded.map(|_| ())
    }

    /// Return `true` if chunk `digest`'s row refers to `object`.
    async fn refers_to(&self, digest: &str, object: &str) -> Result<bool, EngineError> {
        let txn = self
            .storage
            .begin(TxnOptions::default().read_only())
            .await?;
        let table = chunks_table(&txn).await?;
        Ok(rows::get(&txn, &table, &key(&self.namespace, digest))
            .await?
            .is_some_and(|row| text_at(&row, column::OBJECT) == Some(object)))
    }

    /// Return chunk `digest`, from wherever its row says it is, checked
    /// against its hash.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::FileCorrupt`] if the chunk is missing or its
    /// content doesn't match its hash, or a storage or object store error.
    pub(crate) async fn load(&self, digest: &str) -> Result<Vec<u8>, EngineError> {
        let missing = || EngineError::FileCorrupt(format!("chunk {digest} is missing"));
        let row = {
            let txn = self
                .storage
                .begin(TxnOptions::default().read_only())
                .await?;
            let Some(table) = catalog::table(&txn, CHUNKS).await? else {
                return Err(missing());
            };
            rows::get(&txn, &table, &key(&self.namespace, digest))
                .await?
                .ok_or_else(missing)?
        };
        let bytes = match (
            row.get(column::BYTES),
            text_at(&row, column::STORE),
            text_at(&row, column::OBJECT),
        ) {
            (Some(Datum::Bytea(bytes)), _, _) => bytes.clone(),
            (_, Some(store), Some(object)) => {
                self.stores.get(store, object).await?.ok_or_else(missing)?
            }
            _ => return Err(missing()),
        };
        if hash(&bytes) != digest {
            return Err(EngineError::FileCorrupt(format!(
                "chunk {digest} doesn't match its hash"
            )));
        }
        Ok(bytes)
    }
}

/// Delete every chunk of `namespace`, when its files are dropped; objects
/// go to the trash, for collection to delete.
///
/// # Errors
///
/// Returns a storage error.
pub(crate) async fn drop_namespace_chunks(txn: &Txn, namespace: &str) -> Result<(), EngineError> {
    let Some(table) = catalog::table(txn, CHUNKS).await? else {
        return Ok(());
    };
    let mut scan = rows::RowScan::prefix(vec![Datum::Text(namespace.to_owned())]);
    loop {
        let batch = rows::scan(txn, &table, &scan).await?;
        for row in &batch.rows {
            trash_object(txn, row).await?;
            rows::delete(txn, &table, &row[..2]).await?;
        }
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => return Ok(()),
        }
    }
}
