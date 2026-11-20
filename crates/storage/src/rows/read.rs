//! Row reads, table and index scans, and the index integrity check.

use std::collections::BTreeMap;
use std::ops::Bound;

use bytes::Bytes;

use super::keys::{
    index_entry, index_range, index_values_prefix, pk_from_index_entry, pk_key, row_key,
};
use super::write::Row;
use crate::catalog::{IndexDef, IndexId, TableDef};
use crate::encoding::{Datum, Direction, SortOrder, decode_row};
use crate::scan::{DEFAULT_SCAN_LIMIT_ROWS, Key, KeyRange, RowFilter, ScanRequest};
use crate::{CatalogError, Result, Txn};
use std::sync::Arc;

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
    /// Bounds on the key column right after `prefix`; `None` for no bounds.
    pub range: Option<ValueRange>,
}

/// Bounds on one key column's values, in value order.
///
/// Rows whose value is NULL are outside every range bounded on at least
/// one side, as comparisons with NULL are never true.
#[derive(Clone, Debug, PartialEq)]
pub struct ValueRange {
    /// The lower bound.
    pub lower: Bound<Datum>,
    /// The upper bound.
    pub upper: Bound<Datum>,
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
            range: None,
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
    scan_with(txn, table, scan, None).await
}

/// Scan one batch like [`scan`], keeping only rows `filter` accepts.
///
/// The filter runs on the storage thread on each encoded row (decode it
/// with [`decode_row`](crate::encoding::decode_row) and
/// [`TableDef::types`]), so rejected rows never leave the storage pool.
/// It must be pure and must not fail on any data (ADR-0006); rejected rows
/// still enter the read set.
///
/// # Errors
///
/// As for [`scan`].
pub async fn scan_filtered(
    txn: &Txn,
    table: &TableDef,
    scan: &RowScan,
    filter: Arc<dyn RowFilter>,
) -> Result<RowBatch> {
    scan_with(txn, table, scan, Some(filter)).await
}

/// Scan one batch of each of several table scans in one storage call.
///
/// Returns one batch per scan, in order, exactly as [`scan`] would for
/// each. Index lookups for a batch of primary key prefixes use this.
///
/// # Errors
///
/// As for [`scan`].
pub async fn scan_many(txn: &Txn, table: &TableDef, scans: &[RowScan]) -> Result<Vec<RowBatch>> {
    let requests = scans
        .iter()
        .map(|scan| table_request(table, scan))
        .collect::<Result<Vec<_>>>()?;
    let types = table.types();
    txn.scan_many(requests)
        .await?
        .into_iter()
        .map(|batch| {
            let rows = batch
                .rows
                .iter()
                .map(|(_, value)| Ok(decode_row(value, &types)?.1))
                .collect::<Result<_>>()?;
            Ok(RowBatch {
                rows,
                resume: batch.resume,
            })
        })
        .collect()
}

/// Scan one batch of each of several index scans in one storage call.
///
/// Returns one batch per scan, in order, exactly as [`scan_index`] would
/// for each. Index lookups for a batch of indexed values use this, then
/// fetch the rows with one [`get_many`].
///
/// # Errors
///
/// As for [`scan_index`].
pub async fn scan_index_many(
    txn: &Txn,
    table: &TableDef,
    index: &IndexDef,
    scans: &[RowScan],
) -> Result<Vec<IndexBatch>> {
    let requests = scans
        .iter()
        .map(|scan| index_request(table, index, scan))
        .collect::<Result<Vec<_>>>()?;
    txn.scan_many(requests)
        .await?
        .into_iter()
        .map(|batch| {
            let primary_keys = batch
                .rows
                .iter()
                .map(|(key, value)| pk_from_index_entry(table, index, key, value))
                .collect::<Result<_>>()?;
            Ok(IndexBatch {
                primary_keys,
                resume: batch.resume,
            })
        })
        .collect()
}

/// Build the storage request for one batch of a table scan.
fn table_request(table: &TableDef, scan: &RowScan) -> Result<ScanRequest> {
    let prefix = pk_key(table, &scan.prefix)?;
    if scan.range.is_some() && scan.prefix.len() >= table.primary_key().len() {
        return Err(CatalogError::InvalidDefinition(
            "a range needs a primary key column after the prefix".into(),
        )
        .into());
    }
    let range = key_range(
        &prefix,
        |v| {
            pk_key(
                table,
                &[scan.prefix.as_slice(), std::slice::from_ref(v)].concat(),
            )
        },
        SortOrder::ASC,
        scan.range.as_ref(),
    )?;
    Ok(request(range, scan))
}

/// Build the storage request for one batch of an index scan.
fn index_request(table: &TableDef, index: &IndexDef, scan: &RowScan) -> Result<ScanRequest> {
    let prefix = index_values_prefix(table, index, &scan.prefix)?;
    let order = match (&scan.range, index.columns().get(scan.prefix.len())) {
        (None, _) => SortOrder::ASC,
        (Some(_), Some(column)) => column.order,
        (Some(_), None) => {
            return Err(CatalogError::InvalidDefinition(format!(
                "a range on index \"{}\" needs a column after the prefix",
                index.name()
            ))
            .into());
        }
    };
    let range = key_range(
        &prefix,
        |v| {
            index_values_prefix(
                table,
                index,
                &[scan.prefix.as_slice(), std::slice::from_ref(v)].concat(),
            )
        },
        order,
        scan.range.as_ref(),
    )?;
    Ok(request(range, scan))
}

/// Scan one batch, optionally filtered on the storage thread.
async fn scan_with(
    txn: &Txn,
    table: &TableDef,
    scan: &RowScan,
    filter: Option<Arc<dyn RowFilter>>,
) -> Result<RowBatch> {
    let mut request = table_request(table, scan)?;
    request.filter = filter;
    let batch = txn.scan(request).await?;
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
    let batch = txn.scan(index_request(table, index, scan)?).await?;
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

/// Return the key range of the entries under `prefix` whose next column lies in `range`.
///
/// `encode` returns the key prefix of the entries with a given next value,
/// and `order` is that column's order in the key. Each value's entries
/// form one contiguous block of keys starting with its encoding, so value
/// bounds become key bounds on those blocks (swapped for descending
/// columns), and the block of NULLs is cut off when either side is bounded.
fn key_range(
    prefix: &[u8],
    encode: impl Fn(&Datum) -> Result<Key>,
    order: SortOrder,
    range: Option<&ValueRange>,
) -> Result<KeyRange> {
    let base = KeyRange::prefix(prefix);
    let Some(range) = range else { return Ok(base) };
    if matches!(
        (&range.lower, &range.upper),
        (Bound::Unbounded, Bound::Unbounded)
    ) {
        return Ok(base);
    }
    let Bound::Included(mut start) = base.start.clone() else {
        unreachable!("prefix ranges start inclusively")
    };
    let mut end = base.end.clone();
    let mut empty = false;
    // Every bound produced is an inclusive start or an exclusive end.
    let raise_start = |start: &mut Key, empty: &mut bool, after: Bound<Key>| match after {
        Bound::Included(k) | Bound::Excluded(k) => {
            if k > *start {
                *start = k;
            }
        }
        Bound::Unbounded => *empty = true,
    };
    let lower_end = |end: &mut Bound<Key>, before: Key| {
        if !matches!(end, Bound::Excluded(e) if *e <= before) {
            *end = Bound::Excluded(before);
        }
    };
    // The keys of a value's entries: [block start, block end).
    let block = |v: &Datum| -> Result<(Key, Bound<Key>)> {
        let key = encode(v)?;
        let end = KeyRange::prefix(&key).end;
        Ok((key, end))
    };
    let end_key = |b: Bound<Key>| match b {
        Bound::Excluded(k) | Bound::Included(k) => Some(k),
        Bound::Unbounded => None,
    };
    let descending = order.direction == Direction::Desc;
    let mut some_value = None;
    for (bound, is_lower) in [(&range.lower, true), (&range.upper, false)] {
        let (value, inclusive) = match bound {
            Bound::Included(v) => (v, true),
            Bound::Excluded(v) => (v, false),
            Bound::Unbounded => continue,
        };
        let (block_start, block_end) = block(value)?;
        some_value = Some(block_start.clone());
        // In key order, is this bound a start (keys after it) or an end?
        let is_start = is_lower != descending;
        match (is_start, inclusive) {
            (true, true) => raise_start(&mut start, &mut empty, Bound::Included(block_start)),
            (true, false) => raise_start(&mut start, &mut empty, block_end),
            (false, true) => {
                if let Some(k) = end_key(block_end) {
                    lower_end(&mut end, k);
                }
            }
            (false, false) => lower_end(&mut end, block_start),
        }
    }
    // Cut off the NULL block, which lies at one end of the prefix range.
    let (null_start, null_end) = block(&Datum::Null)?;
    if some_value.is_some_and(|v| null_start < v) {
        raise_start(&mut start, &mut empty, null_end);
    } else {
        lower_end(&mut end, null_start);
    }
    if empty || matches!(&end, Bound::Excluded(e) if *e <= start) {
        return Ok(KeyRange::new(
            Bound::Included(start.clone()),
            Bound::Excluded(start),
        ));
    }
    Ok(KeyRange::new(Bound::Included(start), end))
}

/// Build the storage request for one scan batch over `range`.
fn request(range: KeyRange, scan: &RowScan) -> ScanRequest {
    let mut request = ScanRequest::new(range);
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

/// Register a dependency on the row of `table` with primary key `pk`.
///
/// The transaction's commit then fails with
/// [`Conflict::DependencyChanged`](crate::Conflict::DependencyChanged) if a
/// transaction that committed after its snapshot wrote that row, at every
/// isolation level. The engine uses this for data a transaction's
/// authorization relied on, such as the policy set.
///
/// # Errors
///
/// Returns [`Error::Encode`](crate::Error::Encode) if `pk` does not encode as a key of `table`.
pub fn register_row_dependency(txn: &Txn, table: &TableDef, pk: &[Datum]) -> Result<()> {
    txn.register_dependency(super::keys::pk_key(table, pk)?);
    Ok(())
}

/// Register a dependency on every entry a scan with leading values `prefix` covers.
///
/// With `index`, the range is that index's entries whose leading indexed
/// values are `prefix`; without, it is the table rows whose leading primary
/// key values are. Like [`register_row_dependency`], the commit then fails
/// at every isolation level if a transaction that committed after the
/// snapshot wrote in that range, including a row inserted into it. The
/// executor uses this for the closure rows it reads to maintain a
/// hierarchy, so concurrent membership changes can't leave it inconsistent.
///
/// # Errors
///
/// Returns [`Error::Encode`](crate::Error::Encode) if the prefix does not
/// encode, or a catalog error if it is longer than the index.
pub fn register_scan_dependency(
    txn: &Txn,
    table: &TableDef,
    index: Option<&IndexDef>,
    prefix: &[Datum],
) -> Result<()> {
    let key = match index {
        Some(index) => index_values_prefix(table, index, prefix)?,
        None => super::keys::pk_key(table, prefix)?,
    };
    txn.register_dependency_range(KeyRange::prefix(&key));
    Ok(())
}
