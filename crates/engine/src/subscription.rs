//! Subscriptions: a session's change feed, filtered by its policies (ADR-0040 phase 4).

use interlaken_ir::QualifiedName;
use interlaken_ir::catalog::Catalog;
use interlaken_ir::interp::Datum;
use interlaken_ir::text::print_type;
use interlaken_storage::{CatalogError, Error, HistoryPin, Storage, TxnOptions};

use crate::catalog::load_catalog;
use crate::feed::{Change, ChangeBatch, ChangeFeed, ChangeFilter, ChangeKind};
use crate::session::{Engine, Session};
use crate::{CommitTs, EngineError};

/// Keys looked up in one query.
const KEYS_PER_QUERY: usize = 64;

/// A table's changes as one principal may see them
/// ([`Session::subscribe`](crate::Session::subscribe)).
///
/// For each committed transaction, the subscription reads the changed rows
/// through the session's policies twice: just before the commit and just
/// after it. A row visible after is an insert if it wasn't visible before
/// and an update if it was; a row visible only before is a delete. So a
/// principal sees a row arrive when it becomes readable, and leave when it
/// stops being readable, whether by a change to the row or a delete. Rows
/// carry column masks; reading a column the principal is denied fails as
/// a query would (`42501`).
///
/// A subscription keeps the versions it has yet to read from version GC,
/// so drop it when it's no longer read.
pub struct Subscription {
    /// The subscriber's own session, which reads at past snapshots.
    session: Session,
    /// The engine, for its storage.
    engine: Engine,
    /// The table's keys, as it changes.
    feed: ChangeFeed,
    /// Keeps the versions after the subscription's position.
    pin: HistoryPin,
    /// The table's name.
    table: String,
    /// The table's ID.
    table_id: i32,
    /// The primary key columns' names and IR types, in key order.
    key_columns: Vec<(String, String)>,
    /// The primary key columns' positions in a row.
    key_positions: Vec<usize>,
}

impl std::fmt::Debug for Subscription {
    /// Render the subscription's table and position.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("table", &self.table)
            .field("position", &self.feed.position())
            .finish_non_exhaustive()
    }
}

/// Quote a name as an IR symbol segment.
fn quote(name: &str) -> String {
    format!("|{}|", name.replace('|', "||"))
}

impl Subscription {
    /// Open `session`'s subscription to `table` after `from`, or after the
    /// last visible commit.
    pub(crate) async fn open(
        session: Session,
        table: &str,
        from: Option<CommitTs>,
    ) -> Result<Self, EngineError> {
        let engine = session.engine().clone();
        let storage = engine.storage();
        let position = from.unwrap_or_else(|| storage.visible_ts());
        let pin = storage.pin_history(position)?;
        let txn = storage.begin(TxnOptions::default().read_only()).await?;
        let catalog = load_catalog(&txn).await;
        txn.abort();
        let catalog = catalog?;
        let def = catalog
            .table_by_name(&QualifiedName::from(table))
            .ok_or_else(|| {
                EngineError::Storage(Error::Catalog(CatalogError::NoSuchTable(table.to_owned())))
            })?;
        let key_positions: Vec<usize> = def
            .primary_key
            .iter()
            .map(|c| usize::try_from(c.get()).expect("column positions fit usize"))
            .collect();
        let key_columns = key_positions
            .iter()
            .map(|&i| {
                let column = &def.columns[i];
                (column.name.to_string(), print_type(&column.ty))
            })
            .collect();
        let table_id = i32::try_from(def.id.get()).expect("table IDs fit i32");
        let feed = engine
            .changes(ChangeFilter::tables([table]).keys_only(), Some(position))
            .await?;
        Ok(Self {
            session,
            engine,
            feed,
            pin,
            table: table.to_owned(),
            table_id,
            key_columns,
            key_positions,
        })
    }

    /// Return the commit timestamp of the last transaction delivered or
    /// passed over: resume from it to continue where this subscription
    /// stopped.
    #[must_use]
    pub fn position(&self) -> CommitTs {
        self.feed.position()
    }

    /// Return the next committed transaction's changes the principal may
    /// see, waiting for one; `None` once the subscription has ended after an
    /// error.
    ///
    /// # Errors
    ///
    /// As for [`ChangeFeed::next`], and the errors of reading the rows
    /// through the session's policies, such as `42501` for a denied column.
    /// Each ends the subscription.
    pub async fn next(&mut self) -> Option<Result<ChangeBatch, EngineError>> {
        loop {
            let batch = match self.feed.next().await? {
                Ok(batch) => batch,
                Err(error) => return Some(Err(error)),
            };
            match self.visible(&batch).await {
                Ok(Some(visible)) => return Some(Ok(visible)),
                Ok(None) => {}
                Err(error) => return Some(Err(error)),
            }
        }
    }

    /// Return the changes of `batch` the principal may see, or `None` if
    /// none, and move the history pin up to its commit.
    async fn visible(&mut self, batch: &ChangeBatch) -> Result<Option<ChangeBatch>, EngineError> {
        let mut keys: Vec<Vec<Datum>> = Vec::new();
        for change in &batch.changes {
            if !keys.contains(&change.key) {
                keys.push(change.key.clone());
            }
        }
        let storage = self.engine.storage().clone();
        let before = self
            .read(
                storage.begin_before(batch.commit_ts, TxnOptions::default())?,
                &keys,
            )
            .await?;
        let after = self
            .read(
                storage.begin_at(batch.commit_ts, TxnOptions::default())?,
                &keys,
            )
            .await?;
        self.pin = storage.pin_history(batch.commit_ts)?;
        let mut changes = Vec::new();
        for key in keys {
            let find = |rows: &[(Vec<Datum>, Vec<Datum>)]| {
                rows.iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, row)| row.clone())
            };
            let row = find(&after);
            let kind = match (find(&before).is_some(), &row) {
                (false, None) => continue,
                (false, Some(_)) => ChangeKind::Insert,
                (true, Some(_)) => ChangeKind::Update,
                (true, None) => ChangeKind::Delete,
            };
            changes.push(Change {
                table: self.table.clone(),
                table_id: self.table_id,
                kind,
                key,
                row,
            });
        }
        Ok((!changes.is_empty()).then_some(ChangeBatch {
            commit_ts: batch.commit_ts,
            changes,
        }))
    }

    /// Return the rows with `keys` that the principal sees in `txn`, with their keys.
    async fn read(
        &mut self,
        txn: interlaken_storage::Txn,
        keys: &[Vec<Datum>],
    ) -> Result<Vec<(Vec<Datum>, Vec<Datum>)>, EngineError> {
        self.session.begin_on(txn).await?;
        let mut found = Vec::new();
        let mut result = Ok(());
        for chunk in keys.chunks(KEYS_PER_QUERY) {
            let mut params = Vec::new();
            let mut alternatives = Vec::new();
            for key in chunk {
                let mut terms = Vec::new();
                for ((name, ty), value) in self.key_columns.iter().zip(key) {
                    params.push(value.clone());
                    terms.push(format!(
                        "(= t.{} (param {} {ty}))",
                        quote(name),
                        params.len()
                    ));
                }
                alternatives.push(format!("(and {})", terms.join(" ")));
            }
            let text = format!(
                "(plan (filter (get {} :as t) (or {})))",
                quote(&self.table),
                alternatives.join(" ")
            );
            match self.session.query(&text, params).await {
                Ok(rows) => {
                    for row in rows.rows() {
                        let key = self.key_positions.iter().map(|&i| row[i].clone()).collect();
                        found.push((key, row));
                    }
                }
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        if self.session.in_transaction() {
            self.session.rollback()?;
        }
        result.map(|()| found)
    }
}
