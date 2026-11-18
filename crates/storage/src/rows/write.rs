//! Row writes with index maintenance and constraint checks (ADR-0007).
//!
//! Every write puts the row and all its index entries in the transaction's
//! write set, so they commit atomically. Constraint checks read the latest
//! state, including the current statement's writes, so one statement can't
//! insert the same key twice. Concurrent duplicates write the same key
//! (the row key or an exclusive unique-index entry), so the later committer
//! fails validation at every isolation level.

use bytes::Bytes;

use super::keys::{IndexEntry, index_entry, pk_key, row_key};
use crate::catalog::{TableDef, TableId, system};
use crate::encoding::{Datum, decode_row, encode_row};
use crate::scan::Key;
use crate::{ConstraintViolation, Result, Txn};

/// A table row: one value per column, in column order.
pub type Row = Vec<Datum>;

/// Insert `row` into `table`.
///
/// # Errors
///
/// - [`Error::Row`](crate::Error::Row) if the row has the wrong number of
///   values or a value of the wrong type.
/// - [`Error::Constraint`](crate::Error::Constraint) if a NOT NULL column is
///   NULL, or the primary key or a unique index value already exists.
/// - [`Error::Encode`](crate::Error::Encode) if a key exceeds 8 KiB.
/// - [`Error::ReadOnlyTransaction`](crate::Error::ReadOnlyTransaction) or a
///   storage error.
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
/// let users = catalog::create_table(&txn, TableSpec {
///     name: "users".into(),
///     columns: vec![
///         ColumnDef::new("id", ColumnType::Int8),
///         ColumnDef::new("email", ColumnType::Text),
///     ],
///     primary_key: vec!["id".into()],
/// }).await?;
/// rows::insert(&txn, &users, vec![Datum::Int8(1), Datum::Text("a@example.com".into())]).await?;
/// txn.next_statement();
/// let row = rows::get(&txn, &users, &[Datum::Int8(1)]).await?;
/// assert_eq!(row.unwrap()[1], Datum::Text("a@example.com".into()));
/// txn.commit().await?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub async fn insert(txn: &Txn, table: &TableDef, row: Row) -> Result<()> {
    let encoded = encode_checked(table, &row)?;
    let key = row_key(table, &row)?;
    let entries = index_entries(table, &row)?;
    register_definition(txn, table);

    let mut probes = vec![key.clone()];
    probes.extend(
        entries
            .iter()
            .filter(|(_, e)| e.exclusive)
            .map(|(_, e)| e.key.clone()),
    );
    let found = txn.get_many_latest(probes).await?;
    if found[0].is_some() {
        return Err(primary_key_violation(table));
    }
    let exclusive = entries.iter().filter(|(_, e)| e.exclusive);
    if let Some(((index, _), _)) = exclusive.zip(&found[1..]).find(|(_, f)| f.is_some()) {
        return Err(unique_violation(index));
    }

    txn.put(key, encoded)?;
    for (_, entry) in entries {
        txn.put(entry.key, entry.value)?;
    }
    Ok(())
}

/// Replace the row whose primary key is `pk` with `row`, which may change the primary key.
///
/// Returns `false` if no such row exists.
///
/// # Errors
///
/// As for [`insert`]; uniqueness is checked only for values that change.
pub async fn update(txn: &Txn, table: &TableDef, pk: &[Datum], row: Row) -> Result<bool> {
    let encoded = encode_checked(table, &row)?;
    let old_key = pk_key(table, pk)?;
    register_definition(txn, table);
    let Some(old) = read_latest(txn, table, &old_key).await? else {
        return Ok(false);
    };
    let new_key = row_key(table, &row)?;
    let old_entries = index_entries(table, &old)?;
    let new_entries = index_entries(table, &row)?;

    // Entries that change, and the keys that must not already exist.
    let changed: Vec<(&IndexEntry, &(&str, IndexEntry))> = old_entries
        .iter()
        .map(|(_, e)| e)
        .zip(&new_entries)
        .filter(|(old, (_, new))| *old != new)
        .collect();
    let mut probes = Vec::new();
    if new_key != old_key {
        probes.push(new_key.clone());
    }
    let unique_probes: Vec<&(&str, IndexEntry)> = changed
        .iter()
        .filter(|(old, (_, new))| new.exclusive && old.key != new.key)
        .map(|(_, new)| *new)
        .collect();
    probes.extend(unique_probes.iter().map(|(_, e)| e.key.clone()));
    let found = txn.get_many_latest(probes).await?;
    let mut found = found.iter();
    if new_key != old_key && found.next().is_some_and(Option::is_some) {
        return Err(primary_key_violation(table));
    }
    if let Some(((index, _), _)) = unique_probes
        .iter()
        .copied()
        .zip(found)
        .find(|(_, f)| f.is_some())
    {
        return Err(unique_violation(index));
    }

    if new_key != old_key {
        txn.delete(old_key)?;
    }
    for (old, (_, new)) in changed {
        if old.key != new.key {
            txn.delete(old.key.clone())?;
        }
        txn.put(new.key.clone(), new.value.clone())?;
    }
    txn.put(new_key, encoded)?;
    Ok(true)
}

/// Delete the row whose primary key is `pk`, with its index entries.
///
/// Returns `false` if no such row exists.
///
/// # Errors
///
/// Returns an error if the key can't be encoded, the stored row is corrupt,
/// or the transaction can't write.
pub async fn delete(txn: &Txn, table: &TableDef, pk: &[Datum]) -> Result<bool> {
    let key = pk_key(table, pk)?;
    register_definition(txn, table);
    let Some(old) = read_latest(txn, table, &key).await? else {
        return Ok(false);
    };
    for (_, entry) in index_entries(table, &old)? {
        txn.delete(entry.key)?;
    }
    txn.delete(key)?;
    Ok(true)
}

/// Return the key of the `sys_tables` row that defines `table`.
fn definition_key(table: TableId) -> Key {
    pk_key(&system().tables, &[Datum::Int4(table.get())]).expect("short key")
}

/// Make the transaction depend on `table`'s definition at every isolation level.
///
/// Every DDL change to a table rewrites its `sys_tables` row, so a
/// transaction that wrote rows under an old definition fails validation.
fn register_definition(txn: &Txn, table: &TableDef) {
    if table.id().get() >= TableId::FIRST_USER {
        txn.register_dependency(definition_key(table.id()));
    }
}

/// Validate `row` against `table` and encode it.
fn encode_checked(table: &TableDef, row: &[Datum]) -> Result<Bytes> {
    let encoded = encode_row(table.schema_version(), &table.types(), row)?;
    if let Some(column) = table
        .columns()
        .iter()
        .zip(row)
        .find(|(c, v)| !c.nullable && v.is_null())
        .map(|(c, _)| c)
    {
        return Err(ConstraintViolation::NotNull {
            table: table.name().to_owned(),
            column: column.name.clone(),
        }
        .into());
    }
    Ok(Bytes::from(encoded))
}

/// Return each index's name and the entry of `row` in it.
fn index_entries<'a>(table: &'a TableDef, row: &[Datum]) -> Result<Vec<(&'a str, IndexEntry)>> {
    table
        .indexes()
        .iter()
        .map(|index| Ok((index.name(), index_entry(table, index, row)?)))
        .collect()
}

/// Read and decode the row at `key`, seeing the current statement's writes.
pub(crate) async fn read_latest(txn: &Txn, table: &TableDef, key: &Key) -> Result<Option<Row>> {
    let value = txn
        .get_many_latest(vec![key.clone()])
        .await?
        .pop()
        .flatten();
    value
        .map(|bytes| Ok(decode_row(&bytes, &table.types())?.1))
        .transpose()
}

/// Construct a primary key violation for `table`.
fn primary_key_violation(table: &TableDef) -> crate::Error {
    ConstraintViolation::PrimaryKey {
        table: table.name().to_owned(),
    }
    .into()
}

/// Construct a unique violation for the index called `index`.
fn unique_violation(index: &str) -> crate::Error {
    ConstraintViolation::Unique {
        index: index.to_owned(),
    }
    .into()
}
