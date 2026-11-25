//! File content in chunks addressed by their BLAKE3 hash (ADR-0044).
//!
//! Chunks live in the engine table `engine_fs_chunks`, keyed by namespace
//! and hash, which clients can't query: a chunk is reached only through a
//! file whose metadata the reader may see.

use std::time::{SystemTime, UNIX_EPOCH};

use interlaken_storage::catalog::{self, ColumnDef, TableDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::{FjallStorage, Storage, Txn, TxnOptions, rows};

use crate::EngineError;

/// The table of chunks.
pub(crate) const CHUNKS: &str = "engine_fs_chunks";

/// The size of every chunk but a file's last.
pub const CHUNK_SIZE: usize = 1 << 20;

/// How many times staging a chunk retries after a conflicting commit.
const STAGE_ATTEMPTS: usize = 5;

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
                not_null("bytes", ColumnType::Bytea),
                not_null("written", ColumnType::Int8),
            ],
            primary_key: vec!["namespace".into(), "hash".into()],
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

/// Store `bytes` as a chunk of `namespace` in a transaction of its own,
/// unless an equal chunk is stored already, whose write time is then
/// refreshed so collection keeps it. Return its hash.
///
/// # Errors
///
/// Returns a storage error, or the last serialization failure if every
/// attempt conflicted.
pub(crate) async fn stage(
    storage: &FjallStorage,
    namespace: &str,
    bytes: Vec<u8>,
) -> Result<String, EngineError> {
    let digest = hash(&bytes);
    let mut last = None;
    for _ in 0..STAGE_ATTEMPTS {
        let txn = storage.begin(TxnOptions::default()).await?;
        let table = chunks_table(&txn).await?;
        let row = vec![
            Datum::Text(namespace.to_owned()),
            Datum::Text(digest.clone()),
            Datum::Bytea(bytes.clone()),
            Datum::Int8(now_micros()),
        ];
        if !rows::update(&txn, &table, &key(namespace, &digest), row.clone()).await? {
            rows::insert(&txn, &table, row).await?;
        }
        match txn.commit().await {
            Ok(_) => return Ok(digest),
            Err(e) if e.sqlstate() == "40001" => last = Some(e),
            Err(e) => return Err(e.into()),
        }
    }
    Err(last.map_or_else(
        || EngineError::Catalog("staging a chunk never ran".into()),
        EngineError::from,
    ))
}

/// Return chunk `hash` of `namespace`, checked against its hash.
///
/// # Errors
///
/// Returns [`EngineError::FileCorrupt`] if the chunk is missing or its
/// content doesn't match its hash, or a storage error.
pub(crate) async fn load(
    storage: &FjallStorage,
    namespace: &str,
    digest: &str,
) -> Result<Vec<u8>, EngineError> {
    let txn = storage.begin(TxnOptions::default().read_only()).await?;
    let Some(table) = catalog::table(&txn, CHUNKS).await? else {
        return Err(EngineError::FileCorrupt(format!(
            "chunk {digest} is missing"
        )));
    };
    let row = rows::get(&txn, &table, &key(namespace, digest)).await?;
    match row.and_then(|mut row| row.drain(..).nth(2)) {
        Some(Datum::Bytea(bytes)) if hash(&bytes) == digest => Ok(bytes),
        Some(_) => Err(EngineError::FileCorrupt(format!(
            "chunk {digest} doesn't match its hash"
        ))),
        None => Err(EngineError::FileCorrupt(format!(
            "chunk {digest} is missing"
        ))),
    }
}

/// Delete every chunk of `namespace`, when its files are dropped.
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
            rows::delete(txn, &table, &row[..2]).await?;
        }
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => return Ok(()),
        }
    }
}
