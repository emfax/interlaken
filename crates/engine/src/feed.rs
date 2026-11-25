//! Change feeds: committed row changes, per table, in commit order (ADR-0040).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use interlaken_ir::interp::Datum;
use interlaken_storage::catalog::{self, TableDef, TableId};
use interlaken_storage::encoding::{SortOrder, decode_row, decode_tuple};
use interlaken_storage::{
    CatalogError, ChangeEntry, ChangeWrite, Error, FjallStorage, Prior, Storage, Timestamp,
    TxnOptions,
};

use crate::{CommitTs, EngineError};

/// Change log entries read from storage at a time.
const PAGE: usize = 256;

/// The highest ID of storage's own system tables, whose changes are catalog changes.
const LAST_SYSTEM_TABLE: i32 = 99;

/// Which changes a feed delivers (ADR-0040).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeFilter {
    /// The tables, by name; `None` for every user table.
    tables: Option<Vec<String>>,
    /// Whether changes carry the new row.
    rows: bool,
}

impl ChangeFilter {
    /// Deliver changes to the named tables.
    ///
    /// # Examples
    ///
    /// ```
    /// let filter = interlaken_engine::ChangeFilter::tables(["orders", "customers"]);
    /// ```
    #[must_use]
    pub fn tables<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            tables: Some(names.into_iter().map(Into::into).collect()),
            rows: true,
        }
    }

    /// Deliver changes to every user table, but not the engine's own tables.
    #[must_use]
    pub fn all() -> Self {
        Self {
            tables: None,
            rows: true,
        }
    }

    /// Leave the new row out of each change: deliver keys and kinds only.
    #[must_use]
    pub fn keys_only(mut self) -> Self {
        self.rows = false;
        self
    }
}

/// What happened to a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// The row was inserted.
    Insert,
    /// The row was updated. A row whose primary key changed is a delete of
    /// the old key and an insert of the new one.
    Update,
    /// The row was deleted.
    Delete,
}

/// One changed row.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// The table's name.
    pub table: String,
    /// The table's ID.
    pub table_id: i32,
    /// What happened to the row.
    pub kind: ChangeKind,
    /// The row's primary key values, in key order.
    pub key: Vec<Datum>,
    /// The new row, for inserts and updates unless the filter is
    /// [`keys_only`](ChangeFilter::keys_only), decoded with the table's
    /// current columns: columns added after the change are NULL.
    pub row: Option<Vec<Datum>>,
}

/// One committed transaction's changes to the filtered tables.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeBatch {
    /// The transaction's commit timestamp. Resume a feed from it to
    /// continue after this batch.
    pub commit_ts: CommitTs,
    /// Its changed rows, in table and key order.
    pub changes: Vec<Change>,
}

/// What the feed knows about one table.
#[derive(Clone, Debug)]
struct TableInfo {
    /// The table's definition.
    def: TableDef,
    /// Whether the filter selects it.
    selected: bool,
}

/// A stream of committed changes ([`Engine::changes`](crate::Engine::changes)).
///
/// It reads below policies, like [`Engine::storage`](crate::Engine::storage):
/// it is for the embedding program's own use, not for clients.
pub struct ChangeFeed {
    /// The storage the change log belongs to.
    storage: FjallStorage,
    /// What to deliver.
    filter: ChangeFilter,
    /// The filtered tables' IDs, by name, as the feed began.
    named: HashMap<String, TableId>,
    /// The last commit delivered or passed over.
    position: Timestamp,
    /// Entries read but not yet handled.
    pending: VecDeque<Arc<ChangeEntry>>,
    /// Table definitions, until the next catalog change.
    tables: HashMap<i32, Option<TableInfo>>,
    /// Set once the feed has failed; it then ends.
    ended: bool,
}

impl std::fmt::Debug for ChangeFeed {
    /// Render the feed's filter and position.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeFeed")
            .field("filter", &self.filter)
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

/// Return `true` for the engine's and storage's own tables.
fn internal(name: &str) -> bool {
    name.starts_with("engine_") || name.starts_with("sys_")
}

impl ChangeFeed {
    /// Open a feed on `storage` delivering `filter`'s changes after `from`,
    /// or after the visible timestamp.
    pub(crate) async fn open(
        storage: FjallStorage,
        filter: ChangeFilter,
        from: Option<CommitTs>,
    ) -> Result<Self, EngineError> {
        let position = from.unwrap_or_else(|| storage.visible_ts());
        let mut named = HashMap::new();
        if let Some(names) = &filter.tables {
            let txn = storage.begin(TxnOptions::default().read_only()).await?;
            for name in names {
                let def = catalog::table(&txn, name).await?.ok_or_else(|| {
                    EngineError::Storage(Error::Catalog(CatalogError::NoSuchTable(name.clone())))
                })?;
                named.insert(name.clone(), def.id());
            }
            txn.abort();
        }
        Ok(Self {
            storage,
            filter,
            named,
            position,
            pending: VecDeque::new(),
            tables: HashMap::new(),
            ended: false,
        })
    }

    /// Return the commit timestamp of the last batch delivered, or of the
    /// last commit passed over because it changed no filtered table: resume
    /// from it to continue where this feed stopped.
    #[must_use]
    pub fn position(&self) -> CommitTs {
        self.position
    }

    /// Return the next committed transaction's changes to the filtered
    /// tables, waiting for one if none is visible yet; `None` once the feed
    /// has ended after an error.
    ///
    /// # Errors
    ///
    /// Each ends the feed:
    /// - a storage error with SQLSTATE `55000` if the change log no longer
    ///   reaches back to the feed's position, or a snapshot install skipped
    ///   commits (resync by reading the tables, then follow from
    ///   [`Session::snapshot_ts`](crate::Session::snapshot_ts)), or if the
    ///   database keeps no change log;
    /// - [`EngineError::TableDropped`] (SQLSTATE `42P01`) if a filtered
    ///   table was dropped or replaced;
    /// - a storage error reading the log or the catalog.
    pub async fn next(&mut self) -> Option<Result<ChangeBatch, EngineError>> {
        if self.ended {
            return None;
        }
        let result = self.advance().await;
        if result.is_err() {
            self.ended = true;
        }
        Some(result)
    }

    /// Read and handle entries until one yields a batch.
    async fn advance(&mut self) -> Result<ChangeBatch, EngineError> {
        loop {
            if let Some(entry) = self.pending.pop_front() {
                let batch = self.handle(&entry).await?;
                self.position = entry.ts;
                if let Some(batch) = batch {
                    return Ok(batch);
                }
                continue;
            }
            let entries = self.storage.read_changes(self.position, PAGE).await?;
            if entries.is_empty() {
                self.storage.wait_after(self.position).await;
                continue;
            }
            self.pending.extend(entries);
        }
    }

    /// Return the batch of `entry`'s filtered changes, or `None` if it
    /// changed no filtered table.
    async fn handle(&mut self, entry: &ChangeEntry) -> Result<Option<ChangeBatch>, EngineError> {
        if entry
            .writes
            .iter()
            .any(|w| w.table_id() <= LAST_SYSTEM_TABLE)
        {
            self.catalog_changed().await?;
        }
        let mut changes = Vec::new();
        for write in &entry.writes {
            let table_id = write.table_id();
            if table_id <= LAST_SYSTEM_TABLE {
                continue;
            }
            let Some(info) = self.table(table_id).await? else {
                continue;
            };
            if !info.selected {
                continue;
            }
            if let Some(change) = decode(&info.def, write, self.filter.rows)? {
                changes.push(change);
            }
        }
        Ok((!changes.is_empty()).then_some(ChangeBatch {
            commit_ts: entry.ts,
            changes,
        }))
    }

    /// Forget table definitions after a catalog change, and end the feed
    /// if a filtered table is gone or now has another ID.
    async fn catalog_changed(&mut self) -> Result<(), EngineError> {
        self.tables.clear();
        if self.named.is_empty() {
            return Ok(());
        }
        let txn = self
            .storage
            .begin(TxnOptions::default().read_only())
            .await?;
        for (name, id) in &self.named {
            let current = catalog::table(&txn, name).await?;
            if current.is_none_or(|def| def.id() != *id) {
                txn.abort();
                return Err(EngineError::TableDropped(name.clone()));
            }
        }
        txn.abort();
        Ok(())
    }

    /// Return what the feed knows about table `id`, loading every table's
    /// definition from the current catalog if it isn't known; `None` if no
    /// such table exists now.
    async fn table(&mut self, id: i32) -> Result<Option<TableInfo>, EngineError> {
        if let Some(info) = self.tables.get(&id) {
            return Ok(info.clone());
        }
        let txn = self
            .storage
            .begin(TxnOptions::default().read_only())
            .await?;
        let defs = catalog::tables(&txn).await?;
        txn.abort();
        for def in defs {
            let selected = match &self.filter.tables {
                Some(_) => self.named.values().any(|named| *named == def.id()),
                None => !internal(def.name()),
            };
            self.tables
                .insert(def.id().get(), Some(TableInfo { def, selected }));
        }
        // Remembered as missing until the next catalog change.
        Ok(self.tables.entry(id).or_insert(None).clone())
    }
}

/// Decode one row write of `table`, or return `None` for a write that
/// changed nothing: a row the transaction inserted and then deleted.
fn decode(
    table: &TableDef,
    write: &ChangeWrite,
    rows: bool,
) -> Result<Option<Change>, EngineError> {
    let kind = match (&write.value, write.prior) {
        (Some(_), Prior::Absent) => ChangeKind::Insert,
        (Some(_), _) => ChangeKind::Update,
        (None, Prior::Absent) => return Ok(None),
        (None, _) => ChangeKind::Delete,
    };
    let corrupt = |e: &dyn std::fmt::Display| {
        EngineError::Catalog(format!("change of {}: {e}", table.name()))
    };
    let orders = vec![SortOrder::ASC; 2 + table.primary_key().len()];
    let key = decode_tuple(&write.key, &orders).map_err(|e| corrupt(&e))?;
    let key = key[2..].iter().map(interlaken_exec::from_stored).collect();
    let row = match (&write.value, rows) {
        (Some(value), true) => Some(
            decode_row(value, &table.types())
                .map_err(|e| corrupt(&e))?
                .1
                .iter()
                .map(interlaken_exec::from_stored)
                .collect(),
        ),
        _ => None,
    };
    Ok(Some(Change {
        table: table.name().to_owned(),
        table_id: table.id().get(),
        kind,
        key,
        row,
    }))
}
