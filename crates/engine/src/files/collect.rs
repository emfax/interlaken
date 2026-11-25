//! Collecting chunks no file refers to (ADR-0044 section 6).

use std::collections::HashSet;
use std::time::Duration;

use interlaken_storage::catalog;
use interlaken_storage::encoding::Datum;
use interlaken_storage::rows::{self, RowScan};
use interlaken_storage::{Storage, Txn, TxnOptions};

use super::chunks::CHUNKS;
use super::settings::FILE_NAMESPACES;
use crate::catalog::stored_name;
use crate::{Engine, EngineError};

/// How many times a namespace's collection retries after a conflict.
const ATTEMPTS: usize = 5;

/// What a collection pass removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    /// Chunks deleted.
    pub chunks: u64,
    /// Their bytes.
    pub bytes: u64,
}

/// Return every row of `table` (a storage table name), or none if it
/// doesn't exist.
async fn all_rows(
    txn: &Txn,
    table: &str,
    prefix: Vec<Datum>,
) -> Result<Vec<Vec<Datum>>, EngineError> {
    let Some(table) = catalog::table(txn, table).await? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut scan = RowScan::prefix(prefix);
    loop {
        let batch = rows::scan(txn, &table, &scan).await?;
        out.extend(batch.rows);
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => return Ok(out),
        }
    }
}

/// Delete `namespace`'s chunks that no node refers to and that were last
/// written before `cutoff` (microseconds since the epoch), in `txn`.
async fn collect_namespace(
    txn: &Txn,
    namespace: &str,
    cutoff: i64,
) -> Result<Collected, EngineError> {
    let mut referenced = HashSet::new();
    for node in all_rows(txn, &stored_name(namespace, "fs_nodes"), Vec::new()).await? {
        if let Some(Datum::Array(chunks)) = node.get(8) {
            for chunk in chunks {
                if let Datum::Text(hash) = chunk {
                    referenced.insert(hash.clone());
                }
            }
        }
    }
    let Some(table) = catalog::table(txn, CHUNKS).await? else {
        return Ok(Collected::default());
    };
    let mut collected = Collected::default();
    for chunk in all_rows(txn, CHUNKS, vec![Datum::Text(namespace.to_owned())]).await? {
        let (Some(Datum::Text(hash)), Some(Datum::Bytea(bytes)), Some(Datum::Int8(written))) =
            (chunk.get(1), chunk.get(2), chunk.get(3))
        else {
            continue;
        };
        if referenced.contains(hash) || *written >= cutoff {
            continue;
        }
        rows::delete(txn, &table, &chunk[..2]).await?;
        collected.chunks += 1;
        collected.bytes += bytes.len() as u64;
    }
    Ok(collected)
}

impl Engine {
    /// Delete file chunks that no file or directory refers to and that
    /// weren't written in the last `grace`, in every namespace with files
    /// (ADR-0044). A writer that hasn't finished keeps its chunks while
    /// they are younger than `grace`, so `grace` must be longer than any
    /// write, and the transaction it finishes in, takes; the book suggests an
    /// hour.
    ///
    /// Each namespace is collected in a transaction of its own, which
    /// conflicts with a concurrent writer reusing a chunk it would delete,
    /// and is retried.
    ///
    /// # Errors
    ///
    /// Returns a storage error, or a serialization failure if every
    /// attempt for a namespace conflicted.
    pub async fn collect_files(&self, grace: Duration) -> Result<Collected, EngineError> {
        let grace = i64::try_from(grace.as_micros()).unwrap_or(i64::MAX);
        let cutoff = super::chunks::now_micros().saturating_sub(grace);
        let storage = &self.inner.storage;
        let namespaces: Vec<String> = {
            let txn = storage.begin(TxnOptions::default().read_only()).await?;
            all_rows(&txn, FILE_NAMESPACES, Vec::new())
                .await?
                .into_iter()
                .filter_map(|row| match row.into_iter().next() {
                    Some(Datum::Text(namespace)) => Some(namespace),
                    _ => None,
                })
                .collect()
        };
        let mut total = Collected::default();
        for namespace in namespaces {
            let mut attempt = 0;
            loop {
                attempt += 1;
                let txn = storage.begin(TxnOptions::default()).await?;
                let collected = collect_namespace(&txn, &namespace, cutoff).await?;
                match txn.commit().await {
                    Ok(_) => {
                        total.chunks += collected.chunks;
                        total.bytes += collected.bytes;
                        break;
                    }
                    Err(e) if e.sqlstate() == "40001" && attempt < ATTEMPTS => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(total)
    }
}
