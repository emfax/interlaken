//! Batched row writes (milestone F3, backlog `batched-row-writes`).
//!
//! [`insert_many`], [`update_many`] and [`delete_many`] behave exactly like
//! calling [`insert`](super::insert), [`update`](super::update) and
//! [`delete`](super::delete) once per row, in order, but make their
//! constraint-check and old-row reads in one `get_many` per batch (two for
//! updates: old rows, then the keys their new values must not collide
//! with). Rows are then written one at a time against an overlay of what
//! the batch has written so far, so a later row sees an earlier row's
//! writes, exactly as the single-row functions see the current statement's
//! writes: two rows of one batch with the same key still collide, and the
//! first failing row decides the error.

use std::collections::HashMap;

use bytes::Bytes;

use super::keys::{IndexEntry, pk_key, row_key};
use super::write::{
    Row, encode_checked, index_entries, primary_key_violation, register_definition,
    unique_violation,
};
use crate::catalog::TableDef;
use crate::encoding::{Datum, decode_row};
use crate::scan::Key;
use crate::{Result, Txn};

/// The latest state of the keys a batch reads: its own writes over what was prefetched.
struct Overlay<'a> {
    /// The transaction.
    txn: &'a Txn,
    /// Values read before the batch wrote anything, seeing the current statement's writes.
    fetched: HashMap<Key, Option<Bytes>>,
    /// What the batch has written so far (`None` for a delete).
    written: HashMap<Key, Option<Bytes>>,
}

impl<'a> Overlay<'a> {
    /// Construct an empty overlay over `txn`.
    fn new(txn: &'a Txn) -> Self {
        Self {
            txn,
            fetched: HashMap::new(),
            written: HashMap::new(),
        }
    }

    /// Read `keys` in one storage call, skipping keys already known.
    async fn prefetch(&mut self, keys: Vec<Key>) -> Result<()> {
        let mut missing: Vec<Key> = Vec::with_capacity(keys.len());
        for key in keys {
            if !self.fetched.contains_key(&key) && !self.written.contains_key(&key) {
                self.fetched.insert(key.clone(), None);
                missing.push(key);
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let values = self.txn.get_many_latest(missing.clone()).await?;
        for (key, value) in missing.into_iter().zip(values) {
            self.fetched.insert(key, value);
        }
        Ok(())
    }

    /// Return the latest value of each of `keys`, reading any not yet known in one storage call.
    async fn get(&mut self, keys: &[Key]) -> Result<Vec<Option<Bytes>>> {
        let unknown: Vec<Key> = keys
            .iter()
            .filter(|k| !self.written.contains_key(*k) && !self.fetched.contains_key(*k))
            .cloned()
            .collect();
        self.prefetch(unknown).await?;
        Ok(keys
            .iter()
            .map(|k| {
                self.written
                    .get(k)
                    .or_else(|| self.fetched.get(k))
                    .cloned()
                    .flatten()
            })
            .collect())
    }

    /// Buffer a write of `value` at `key`.
    fn put(&mut self, key: Key, value: Bytes) -> Result<()> {
        self.txn.put(key.clone(), value.clone())?;
        self.written.insert(key, Some(value));
        Ok(())
    }

    /// Buffer a delete of `key`.
    fn delete(&mut self, key: Key) -> Result<()> {
        self.txn.delete(key.clone())?;
        self.written.insert(key, None);
        Ok(())
    }
}

/// What an insert writes for one row, computed before any read.
struct PreparedInsert<'t> {
    /// The encoded row.
    encoded: Bytes,
    /// The row key.
    key: Key,
    /// Each index's name and the row's entry in it.
    entries: Vec<(&'t str, IndexEntry)>,
}

/// Encode one row for insertion, as [`insert`](super::insert) does before reading.
fn prepare_insert<'t>(table: &'t TableDef, row: &[Datum]) -> Result<PreparedInsert<'t>> {
    let encoded = encode_checked(table, row)?;
    let key = row_key(table, row)?;
    let entries = index_entries(table, row)?;
    Ok(PreparedInsert {
        encoded,
        key,
        entries,
    })
}

/// Insert `rows` into `table`, in order, as [`insert`](super::insert) would one at a time.
///
/// Every key that must not exist yet is read in one storage call.
///
/// # Errors
///
/// The error [`insert`](super::insert) would return for the first row
/// that fails, including a row whose key an earlier row of `rows` took.
/// Rows before it are written (the caller's statement rolls back).
///
/// # Examples
///
/// ```
/// use interlaken_storage::catalog::{self, ColumnDef, TableSpec};
/// use interlaken_storage::encoding::{ColumnType, Datum};
/// use interlaken_storage::{FjallStorage, Storage, StorageOptions, TxnOptions, rows};
///
/// # tokio::runtime::Runtime::new()?.block_on(async {
/// let dir = tempfile::tempdir()?;
/// let storage = FjallStorage::open(dir.path(), StorageOptions::default())?;
/// let txn = storage.begin(TxnOptions::default()).await?;
/// let t = catalog::create_table(&txn, TableSpec {
///     name: "t".into(),
///     columns: vec![ColumnDef::new("id", ColumnType::Int8)],
///     primary_key: vec!["id".into()],
/// }).await?;
/// rows::insert_many(&txn, &t, vec![vec![Datum::Int8(1)], vec![Datum::Int8(2)]]).await?;
/// // A batch whose rows collide fails like two single inserts would.
/// assert!(rows::insert_many(&txn, &t, vec![vec![Datum::Int8(3)], vec![Datum::Int8(3)]]).await.is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub async fn insert_many(txn: &Txn, table: &TableDef, rows: Vec<Row>) -> Result<()> {
    let prepared: Vec<Result<PreparedInsert<'_>>> =
        rows.iter().map(|row| prepare_insert(table, row)).collect();
    if prepared.first().is_some_and(Result::is_ok) {
        register_definition(txn, table);
    }
    let mut overlay = Overlay::new(txn);
    // Rows after the first one that fails to encode are never checked.
    let probes: Vec<Key> = prepared
        .iter()
        .map_while(|p| p.as_ref().ok())
        .flat_map(|p| {
            std::iter::once(p.key.clone()).chain(
                p.entries
                    .iter()
                    .filter(|(_, e)| e.exclusive)
                    .map(|(_, e)| e.key.clone()),
            )
        })
        .collect();
    overlay.prefetch(probes).await?;
    for p in prepared {
        let p = p?;
        let mut probes = vec![p.key.clone()];
        probes.extend(
            p.entries
                .iter()
                .filter(|(_, e)| e.exclusive)
                .map(|(_, e)| e.key.clone()),
        );
        let found = overlay.get(&probes).await?;
        if found[0].is_some() {
            return Err(primary_key_violation(table));
        }
        let exclusive = p.entries.iter().filter(|(_, e)| e.exclusive);
        if let Some(((index, _), _)) = exclusive.zip(&found[1..]).find(|(_, f)| f.is_some()) {
            return Err(unique_violation(index));
        }
        overlay.put(p.key, p.encoded)?;
        for (_, entry) in p.entries {
            overlay.put(entry.key, entry.value)?;
        }
    }
    Ok(())
}

/// Delete the rows whose primary keys are `pks`, in order, as [`delete`](super::delete) would.
///
/// Returns, for each key, whether a row was deleted. The old rows are read
/// in one storage call.
///
/// # Errors
///
/// As for [`delete`](super::delete), for the first key that fails.
pub async fn delete_many(txn: &Txn, table: &TableDef, pks: &[Vec<Datum>]) -> Result<Vec<bool>> {
    let keys: Vec<Result<Key>> = pks.iter().map(|pk| pk_key(table, pk)).collect();
    if keys.first().is_some_and(Result::is_ok) {
        register_definition(txn, table);
    }
    let mut overlay = Overlay::new(txn);
    overlay
        .prefetch(
            keys.iter()
                .map_while(|k| k.as_ref().ok())
                .cloned()
                .collect(),
        )
        .await?;
    let types = table.types();
    let mut deleted = Vec::with_capacity(keys.len());
    for key in keys {
        let key = key?;
        let Some(bytes) = overlay
            .get(std::slice::from_ref(&key))
            .await?
            .pop()
            .flatten()
        else {
            deleted.push(false);
            continue;
        };
        let old = decode_row(&bytes, &types)?.1;
        for (_, entry) in index_entries(table, &old)? {
            overlay.delete(entry.key)?;
        }
        overlay.delete(key)?;
        deleted.push(true);
    }
    Ok(deleted)
}

/// Return the keys an update of the row `old` to `row` must find absent.
fn update_probes(
    old_key: &Key,
    new_key: &Key,
    old_entries: &[(&str, IndexEntry)],
    new_entries: &[(&str, IndexEntry)],
) -> Vec<Key> {
    let mut probes = Vec::new();
    if new_key != old_key {
        probes.push(new_key.clone());
    }
    probes.extend(
        old_entries
            .iter()
            .zip(new_entries)
            .filter(|((_, old), (_, new))| old != new && new.exclusive && old.key != new.key)
            .map(|(_, (_, new))| new.key.clone()),
    );
    probes
}

/// Replace rows, in order, as [`update`](super::update) would one `(pk, row)` pair at a time.
///
/// Returns, for each pair, whether its row existed (and was replaced). The
/// old rows are read in one storage call, then the keys the new values
/// must not collide with in another.
///
/// # Errors
///
/// As for [`update`](super::update), for the first pair that fails,
/// including a new key that an earlier pair of the batch took.
pub async fn update_many(
    txn: &Txn,
    table: &TableDef,
    updates: Vec<(Vec<Datum>, Row)>,
) -> Result<Vec<bool>> {
    let prepared: Vec<Result<(Bytes, Key)>> = updates
        .iter()
        .map(|(pk, row)| Ok((encode_checked(table, row)?, pk_key(table, pk)?)))
        .collect();
    if prepared.first().is_some_and(Result::is_ok) {
        register_definition(txn, table);
    }
    let mut overlay = Overlay::new(txn);
    let ready: Vec<(&(Bytes, Key), &Row)> = prepared
        .iter()
        .map_while(|p| p.as_ref().ok())
        .zip(updates.iter().map(|(_, row)| row))
        .collect();
    overlay
        .prefetch(ready.iter().map(|((_, key), _)| key.clone()).collect())
        .await?;
    // The collision probes as of the rows read; any a changed row needs
    // beyond these are read when it is written.
    let types = table.types();
    let mut probes = Vec::new();
    for ((_, old_key), row) in &ready {
        let Some(bytes) = overlay
            .get(std::slice::from_ref(old_key))
            .await?
            .pop()
            .flatten()
        else {
            continue;
        };
        let Ok((_, old)) = decode_row(&bytes, &types) else {
            continue;
        };
        if let (Ok(new_key), Ok(old_entries), Ok(new_entries)) = (
            row_key(table, row),
            index_entries(table, &old),
            index_entries(table, row),
        ) {
            probes.extend(update_probes(old_key, &new_key, &old_entries, &new_entries));
        }
    }
    overlay.prefetch(probes).await?;
    let mut replaced = Vec::with_capacity(updates.len());
    for (p, (_, row)) in prepared.into_iter().zip(updates) {
        let (encoded, old_key) = p?;
        let Some(bytes) = overlay
            .get(std::slice::from_ref(&old_key))
            .await?
            .pop()
            .flatten()
        else {
            replaced.push(false);
            continue;
        };
        let old = decode_row(&bytes, &types)?.1;
        let new_key = row_key(table, &row)?;
        let old_entries = index_entries(table, &old)?;
        let new_entries = index_entries(table, &row)?;
        let probes = update_probes(&old_key, &new_key, &old_entries, &new_entries);
        let found = overlay.get(&probes).await?;
        let mut found = found.iter();
        if new_key != old_key && found.next().is_some_and(Option::is_some) {
            return Err(primary_key_violation(table));
        }
        let unique_probes = old_entries
            .iter()
            .zip(&new_entries)
            .filter(|((_, old), (_, new))| old != new && new.exclusive && old.key != new.key)
            .map(|(_, (index, _))| *index);
        if let Some((index, _)) = unique_probes.zip(found).find(|(_, f)| f.is_some()) {
            return Err(unique_violation(index));
        }
        if new_key != old_key {
            overlay.delete(old_key)?;
        }
        for ((_, old), (_, new)) in old_entries.iter().zip(&new_entries) {
            if old == new {
                continue;
            }
            if old.key != new.key {
                overlay.delete(old.key.clone())?;
            }
            overlay.put(new.key.clone(), new.value.clone())?;
        }
        overlay.put(new_key, encoded)?;
        replaced.push(true);
    }
    Ok(replaced)
}
