//! Row reads, table and index scans, and the index integrity check.

use std::collections::BTreeMap;

use bytes::Bytes;

use super::keys::{
    index_entry, index_range, index_values_prefix, pk_from_index_entry, pk_key, row_key,
};
use super::write::Row;
use crate::catalog::{IndexDef, IndexId, TableDef};
use crate::encoding::{Datum, decode_row};
use crate::scan::{DEFAULT_SCAN_LIMIT_ROWS, Key, KeyRange, ScanRequest};
use crate::{Result, Txn};

/// Return the row whose primary key is `pk`, or `None`.
///
/// Like [`Txn::get_many`], this sees the snapshot plus writes from earlier
/// statements, and the key enters the read set.
///
/// # Errors
///
/// Returns an error if the key can't be encoded, the stored row is corrupt,
/// or the storage read fails.
pub async fn get(txn: &Txn, table: &TableDef, pk: &[Datum]) -> Result<Option<Row>> {
    Ok(get_many(txn, table, &[pk.to_vec()]).await?.pop().flatten())
}

/// Return the rows with the given primary keys, in order, in one storage call.
///
/// # Errors
///
/// As for [`get`].
pub async fn get_many(txn: &Txn, table: &TableDef, pks: &[Vec<Datum>]) -> Result<Vec<Option<Row>>> {
    let keys = pks
        .iter()
        .map(|pk| pk_key(table, pk))
        .collect::<Result<Vec<_>>>()?;
    let types = table.types();
    txn.get_many(keys)
        .await?
        .into_iter()
        .map(|value| {
            value
                .map(|bytes| Ok(decode_row(&bytes, &types)?.1))
                .transpose()
        })
        .collect()
}

/// One batch of a table or index scan.
#[derive(Clone, Debug, PartialEq)]
pub struct RowScan {
    /// Leading primary key values (table scan) or indexed values (index
    /// scan) every result must have; empty to scan everything.
    pub prefix: Vec<Datum>,
    /// Scan in descending key order.
    pub reverse: bool,
    /// Maximum entries per batch.
    pub limit_rows: usize,
    /// Resume key from the previous batch of the same scan.
    pub resume: Option<Key>,
}

impl RowScan {
    /// Construct a forward scan of everything with the default row limit.
    #[must_use]
    pub fn all() -> Self {
        Self::prefix(Vec::new())
    }

    /// Construct a forward scan of the entries starting with `prefix`.
    #[must_use]
    pub fn prefix(prefix: Vec<Datum>) -> Self {
        Self {
            prefix,
            reverse: false,
            limit_rows: DEFAULT_SCAN_LIMIT_ROWS,
            resume: None,
        }
    }
}

/// The result of one [`scan`] call.
#[derive(Clone, Debug, PartialEq)]
pub struct RowBatch {
    /// Rows in key order.
    pub rows: Vec<Row>,
    /// Where to continue; `None` when the scan is complete.
    pub resume: Option<Key>,
}

/// The result of one [`scan_index`] call.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexBatch {
    /// Primary keys of the matching rows, in index order.
    pub primary_keys: Vec<Vec<Datum>>,
    /// Where to continue; `None` when the scan is complete.
    pub resume: Option<Key>,
}

/// Scan one batch of `table`'s rows in primary key order.
///
/// # Errors
///
/// Returns an error if the prefix can't be encoded, a stored row is
/// corrupt, or the storage read fails.
pub async fn scan(txn: &Txn, table: &TableDef, scan: &RowScan) -> Result<RowBatch> {
    let prefix = pk_key(table, &scan.prefix)?;
    let batch = txn.scan(request(&prefix, scan)).await?;
    let types = table.types();
    let rows = batch
        .rows
        .iter()
        .map(|(_, value)| Ok(decode_row(value, &types)?.1))
        .collect::<Result<_>>()?;
    Ok(RowBatch {
        rows,
        resume: batch.resume,
    })
}

/// Scan one batch of `index` in index order, returning primary keys.
///
/// Fetch the rows with [`get_many`].
///
/// # Errors
///
/// Returns an error if the prefix is longer than the index or can't be
/// encoded, an entry is corrupt, or the storage read fails.
pub async fn scan_index(
    txn: &Txn,
    table: &TableDef,
    index: &IndexDef,
    scan: &RowScan,
) -> Result<IndexBatch> {
    let prefix = index_values_prefix(table, index, &scan.prefix)?;
    let batch = txn.scan(request(&prefix, scan)).await?;
    let primary_keys = batch
        .rows
        .iter()
        .map(|(key, value)| pk_from_index_entry(table, index, key, value))
        .collect::<Result<_>>()?;
    Ok(IndexBatch {
        primary_keys,
        resume: batch.resume,
    })
}

/// Build the storage request for one scan batch under `prefix`.
fn request(prefix: &[u8], scan: &RowScan) -> ScanRequest {
    let mut request = ScanRequest::new(KeyRange::prefix(prefix));
    request.reverse = scan.reverse;
    request.limit_rows = scan.limit_rows;
    request.resume.clone_from(&scan.resume);
    request
}

/// Read every `(key, value)` in `range`, seeing the current statement's writes.
pub(crate) async fn read_range_latest(txn: &Txn, range: KeyRange) -> Result<Vec<(Key, Bytes)>> {
    let mut request = ScanRequest::new(range);
    let mut entries = Vec::new();
    loop {
        let mut batch = txn.scan_latest(request.clone()).await?;
        entries.append(&mut batch.rows);
        match request.resume_after(&batch) {
            Some(next) => request = next,
            None => return Ok(entries),
        }
    }
}

/// Check that `table`'s indexes match its rows exactly; return each discrepancy.
///
/// Every row must have exactly its expected entry in every index, and every
/// index must hold no other entries. Reads the whole table and all its
/// indexes at the transaction's snapshot, including all buffered writes.
///
/// # Errors
///
/// Returns an error if a stored row can't be decoded or the storage read fails.
pub async fn check_indexes(txn: &Txn, table: &TableDef) -> Result<Vec<String>> {
    let types = table.types();
    let mut problems = Vec::new();
    let mut expected: Vec<BTreeMap<Key, Bytes>> = vec![BTreeMap::new(); table.indexes().len()];
    for (key, value) in read_range_latest(txn, index_range(table.id(), IndexId::PRIMARY)).await? {
        let row = decode_row(&value, &types)?.1;
        if row_key(table, &row)? != key {
            problems.push(format!("row stored under the wrong key: {row:?}"));
        }
        for (index, expected) in table.indexes().iter().zip(&mut expected) {
            let entry = index_entry(table, index, &row)?;
            if expected.insert(entry.key, entry.value).is_some() {
                problems.push(format!(
                    "two rows share an entry in unique index \"{}\": {row:?}",
                    index.name()
                ));
            }
        }
    }
    for (index, expected) in table.indexes().iter().zip(expected) {
        let actual: BTreeMap<Key, Bytes> =
            read_range_latest(txn, index_range(table.id(), index.id()))
                .await?
                .into_iter()
                .collect();
        for (key, value) in &expected {
            match actual.get(key) {
                None => problems.push(format!("index \"{}\" is missing an entry", index.name())),
                Some(v) if v != value => {
                    problems.push(format!(
                        "index \"{}\" entry has the wrong value",
                        index.name()
                    ));
                }
                Some(_) => {}
            }
        }
        let extra = actual.keys().filter(|k| !expected.contains_key(*k)).count();
        if extra > 0 {
            problems.push(format!(
                "index \"{}\" has {extra} entries without a matching row",
                index.name()
            ));
        }
    }
    Ok(problems)
}
