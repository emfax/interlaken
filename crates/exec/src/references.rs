//! Foreign key checks for DML (ADR-0035).
//!
//! When a statement writes a table that foreign keys involve, the physical
//! planner attaches a [`ReferencePlan`] to the DML operator, and a
//! [`Checker`] checks each batch as it is written:
//!
//! - **Outgoing:** each new referencing key (an inserted row, or an
//!   updated row whose referencing columns changed) must name an existing
//!   referenced row. The probe sees the statement's own writes, so a row
//!   may refer to one inserted earlier in the statement.
//! - **Incoming:** each removed referenced key (a deleted row, or an
//!   updated row whose referenced columns changed) must leave no row that
//!   refers to it.
//!
//! A key that fails its probe isn't an error yet: a later batch of the same
//! statement may insert the missing row or delete the referring rows. Such
//! keys are kept, charged to the query's memory budget, and rechecked when
//! the statement's input is exhausted. Only then does a violation fail the
//! statement, with SQLSTATE `23503`. In the usual case every probe
//! succeeds at once and nothing is kept.
//!
//! Every probe registers what it read as a dependency, so a concurrent
//! write that would break a checked reference fails one of the two
//! transactions at every isolation level. Probes read rows the writer's
//! policies may hide: a constraint can't depend on who is asking.
//!
//! `on delete cascade` and `set-null` aren't carried out yet (ADR-0035
//! phase 3); until then they behave as `no-action`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use interlaken_ir::catalog::{Catalog, ReferentialAction, TableDef, referencing_index};
use interlaken_ir::interp::{Datum, RowKey};
use interlaken_ir::{ColumnId, Ident};
use interlaken_storage::Txn;
use interlaken_storage::catalog::{IndexDef as StoredIndex, TableDef as StoredTable};
use interlaken_storage::encoding::Datum as Stored;
use interlaken_storage::rows;

use crate::error::ExecError;
use crate::memory::{Reservation, key_bytes};
use crate::to_stored;

/// A probe of one table's primary key, or of one of its indexes.
#[derive(Clone, Debug)]
struct Probe {
    /// The probed table's name.
    table: String,
    /// The index probed, or `None` for the primary key.
    index: Option<String>,
    /// Positions in the target row of the probed key's values, in key column order.
    key: Vec<usize>,
    /// The names of the probed key's columns, in the same order, for messages.
    names: Vec<String>,
}

/// A foreign key whose referencing table is the DML target.
#[derive(Clone, Debug)]
struct Outgoing {
    /// The foreign key's name.
    name: String,
    /// Positions of the referencing columns in the target row.
    columns: Vec<usize>,
    /// The referenced key, probed with the target row's referencing values.
    referenced: Probe,
    /// Whether the foreign key matches FULL.
    match_full: bool,
}

/// A foreign key whose referenced table is the DML target.
#[derive(Clone, Debug)]
struct Incoming {
    /// The foreign key's name.
    name: String,
    /// Positions of the referenced columns in the target row.
    columns: Vec<usize>,
    /// The referencing table's index (or primary key) whose leading
    /// columns are the referencing columns, probed with the target row's
    /// referenced values.
    referencing: Probe,
    /// The target's own referenced key, probed to see whether a removed
    /// key was written again by the statement.
    own: Probe,
    /// What deleting a referenced row should do.
    on_delete: ReferentialAction,
}

/// The foreign keys a DML statement on one table must check.
#[derive(Clone, Debug)]
pub struct ReferencePlan {
    /// The target table's name.
    target: String,
    /// Foreign keys from the target table.
    outgoing: Vec<Outgoing>,
    /// Foreign keys to the target table.
    incoming: Vec<Incoming>,
}

/// Return the position of column `id`.
fn at(id: ColumnId) -> usize {
    usize::try_from(id.get()).expect("column id")
}

/// Return the name of the column at `position` of `table`.
fn column_name(table: &TableDef, position: usize) -> String {
    table.columns[position].name.to_string()
}

/// Return the key columns of `table`'s primary key, or of its index `index`.
fn key_columns(table: &TableDef, index: Option<&Ident>) -> Result<Vec<ColumnId>, ExecError> {
    match index {
        None => Ok(table.primary_key.clone()),
        Some(name) => table
            .indexes
            .iter()
            .find(|i| &i.name == name)
            .map(|i| i.columns.iter().map(|c| c.column).collect())
            .ok_or_else(|| ExecError::Catalog(format!("{} has no index {name}", table.name))),
    }
}

impl ReferencePlan {
    /// Return the plan for DML on `table`, or `None` if no foreign key involves it.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Catalog`] if a foreign key names a table or
    /// index the catalog doesn't have, which the catalog's own checks rule out.
    pub fn for_table(catalog: &dyn Catalog, table: &TableDef) -> Result<Option<Self>, ExecError> {
        let lookup = |id| {
            catalog
                .table_by_id(id)
                .ok_or_else(|| ExecError::Catalog(format!("unknown table {}", id.get())))
        };
        let mut outgoing = Vec::new();
        for fk in catalog.foreign_keys_of(table.id) {
            let referenced = lookup(fk.referenced)?;
            let key = key_columns(&referenced, fk.referenced_index.as_ref())?;
            // For each referenced key column, the referencing column paired with it.
            let sources = key
                .iter()
                .map(|k| {
                    let i = fk
                        .referenced_columns
                        .iter()
                        .position(|c| c == k)
                        .ok_or_else(|| ExecError::Catalog(format!("foreign key {}", fk.name)))?;
                    Ok(at(fk.columns[i]))
                })
                .collect::<Result<_, ExecError>>()?;
            outgoing.push(Outgoing {
                name: fk.name.to_string(),
                columns: fk.columns.iter().map(|&c| at(c)).collect(),
                referenced: Probe {
                    table: referenced.name.last().to_string(),
                    index: fk.referenced_index.as_ref().map(ToString::to_string),
                    key: sources,
                    names: key
                        .iter()
                        .map(|&k| column_name(&referenced, at(k)))
                        .collect(),
                },
                match_full: fk.match_full,
            });
        }
        let mut incoming = Vec::new();
        for fk in catalog.foreign_keys_referencing(table.id) {
            let referencing = lookup(fk.table)?;
            let index = referencing_index(&fk, &referencing)
                .ok_or_else(|| ExecError::Catalog(format!("foreign key {} has no index", fk.name)))?
                .cloned();
            let leading = key_columns(&referencing, index.as_ref())?;
            // For each leading referencing column, the target's referenced column paired with it.
            let sources = leading[..fk.columns.len()]
                .iter()
                .map(|c| {
                    let i =
                        fk.columns.iter().position(|x| x == c).ok_or_else(|| {
                            ExecError::Catalog(format!("foreign key {}", fk.name))
                        })?;
                    Ok(at(fk.referenced_columns[i]))
                })
                .collect::<Result<Vec<_>, ExecError>>()?;
            let own_key = key_columns(table, fk.referenced_index.as_ref())?;
            incoming.push(Incoming {
                name: fk.name.to_string(),
                columns: fk.referenced_columns.iter().map(|&c| at(c)).collect(),
                referencing: Probe {
                    table: referencing.name.last().to_string(),
                    index: index.as_ref().map(ToString::to_string),
                    names: sources.iter().map(|&p| column_name(table, p)).collect(),
                    key: sources,
                },
                own: Probe {
                    table: table.name.last().to_string(),
                    index: fk.referenced_index.as_ref().map(ToString::to_string),
                    key: own_key.iter().map(|&k| at(k)).collect(),
                    names: own_key.iter().map(|&k| column_name(table, at(k))).collect(),
                },
                on_delete: fk.on_delete,
            });
        }
        if outgoing.is_empty() && incoming.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            target: table.name.last().to_string(),
            outgoing,
            incoming,
        }))
    }

    /// Return the names of the foreign keys checked, for `EXPLAIN`.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .outgoing
            .iter()
            .map(|o| o.name.clone())
            .chain(self.incoming.iter().map(|i| i.name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Keys awaiting a check, in the order they were found.
#[derive(Default)]
struct Keys {
    /// Each `(foreign key, key values)`, once.
    order: Vec<(usize, RowKey)>,
    /// The same, for deduplication.
    seen: HashSet<(usize, RowKey)>,
}

impl Keys {
    /// Add `key` of foreign key `fk`; return its memory if it was new.
    fn add(&mut self, fk: usize, key: Vec<Datum>) -> Option<usize> {
        let key = RowKey(key);
        if self.seen.contains(&(fk, key.clone())) {
            return None;
        }
        let bytes = 2 * (key_bytes(&key) + std::mem::size_of::<(usize, RowKey)>());
        self.seen.insert((fk, key.clone()));
        self.order.push((fk, key));
        Some(bytes)
    }

    /// Remove and return every key, with the memory they held.
    fn take(&mut self) -> (Vec<(usize, RowKey)>, usize) {
        let bytes = self
            .order
            .iter()
            .map(|(_, k)| 2 * (key_bytes(k) + std::mem::size_of::<(usize, RowKey)>()))
            .sum();
        self.seen.clear();
        (std::mem::take(&mut self.order), bytes)
    }
}

/// Checks one DML statement's writes against its foreign keys.
pub(crate) struct Checker {
    /// What to check.
    plan: Arc<ReferencePlan>,
    /// Stored tables, looked up on first use.
    stored: HashMap<String, StoredTable>,
    /// Outgoing keys to probe at the end of this batch.
    new_keys: Keys,
    /// Incoming keys to probe at the end of this batch.
    removed_keys: Keys,
    /// Outgoing keys that failed their probe, to recheck at the end.
    pending_new: Keys,
    /// Incoming keys that failed their probe, to recheck at the end.
    pending_removed: Keys,
    /// The memory the kept keys hold.
    memory: Reservation,
}

/// Return the values of `row` at `positions`.
fn values(row: &[Datum], positions: &[usize]) -> Vec<Datum> {
    positions.iter().map(|&p| row[p].clone()).collect()
}

/// Convert key values to their stored form.
fn stored_key(key: &RowKey) -> Result<Vec<Stored>, ExecError> {
    key.0
        .iter()
        .map(|d| to_stored(d).ok_or_else(|| ExecError::Unsupported(format!("key value {d:?}"))))
        .collect()
}

/// Render `names` and `key` as `a = 1, b = 2`.
fn shown(names: &[String], key: &RowKey) -> String {
    names
        .iter()
        .zip(&key.0)
        .map(|(n, v)| format!("{n} = {v:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Checker {
    /// Start checking a statement's writes against `plan`, charging kept keys to `memory`.
    pub(crate) fn new(plan: Arc<ReferencePlan>, memory: Reservation) -> Self {
        Self {
            plan,
            stored: HashMap::new(),
            new_keys: Keys::default(),
            removed_keys: Keys::default(),
            pending_new: Keys::default(),
            pending_removed: Keys::default(),
            memory,
        }
    }

    /// Record a written row: `old` is the row before (none for an insert),
    /// `new` the row after (none for a delete).
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::ForeignKey`] for a new row whose referencing
    /// columns are partly NULL under a foreign key that matches FULL, or a
    /// memory error.
    pub(crate) fn record(
        &mut self,
        old: Option<&[Datum]>,
        new: Option<&[Datum]>,
    ) -> Result<(), ExecError> {
        let plan = self.plan.clone();
        if let Some(new) = new {
            for (i, fk) in plan.outgoing.iter().enumerate() {
                let referencing = values(new, &fk.columns);
                if old.is_some_and(|old| values(old, &fk.columns) == referencing) {
                    continue;
                }
                let nulls = referencing.iter().filter(|v| v.is_null()).count();
                if nulls == referencing.len() || (nulls > 0 && !fk.match_full) {
                    continue;
                }
                if nulls > 0 {
                    return Err(ExecError::ForeignKey(format!(
                        "a row of {} has some but not all of the columns of foreign key {}, \
                         which matches FULL, NULL",
                        plan.target, fk.name
                    )));
                }
                if let Some(bytes) = self.new_keys.add(i, values(new, &fk.referenced.key)) {
                    self.memory.grow(bytes)?;
                }
            }
        }
        if let Some(old) = old {
            for (i, fk) in plan.incoming.iter().enumerate() {
                let referenced = values(old, &fk.columns);
                if new.is_some_and(|new| values(new, &fk.columns) == referenced) {
                    continue;
                }
                // A key with a NULL can't be referred to under either match rule.
                if referenced.iter().any(Datum::is_null) {
                    continue;
                }
                if let Some(bytes) = self.removed_keys.add(i, values(old, &fk.referencing.key)) {
                    self.memory.grow(bytes)?;
                }
            }
        }
        Ok(())
    }

    /// Return the stored definition of table `name`.
    async fn stored(&mut self, txn: &Txn, name: &str) -> Result<StoredTable, ExecError> {
        if let Some(table) = self.stored.get(name) {
            return Ok(table.clone());
        }
        let table = interlaken_storage::catalog::table(txn, name)
            .await?
            .ok_or_else(|| ExecError::Catalog(format!("table {name} is not stored")))?;
        self.stored.insert(name.to_owned(), table.clone());
        Ok(table)
    }

    /// Return the stored index `probe` reads, if it reads one, with its table.
    async fn target(
        &mut self,
        txn: &Txn,
        probe: &Probe,
    ) -> Result<(StoredTable, Option<StoredIndex>), ExecError> {
        let table = self.stored(txn, &probe.table).await?;
        let index = match &probe.index {
            None => None,
            Some(name) => Some(
                table
                    .indexes()
                    .iter()
                    .find(|i| i.name() == name)
                    .cloned()
                    .ok_or_else(|| ExecError::Catalog(format!("index {name} is not stored")))?,
            ),
        };
        Ok((table, index))
    }

    /// Return, for each of `keys`, whether `probe`'s key holds it.
    async fn exists(
        &mut self,
        txn: &Txn,
        probe: &Probe,
        keys: &[&RowKey],
    ) -> Result<Vec<bool>, ExecError> {
        let (table, index) = self.target(txn, probe).await?;
        let stored = keys
            .iter()
            .map(|k| stored_key(k))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows::probe_keys(txn, &table, index.as_ref(), &stored).await?)
    }

    /// Return, for each of `keys`, whether any row's `probe` key starts with it.
    async fn referred(
        &mut self,
        txn: &Txn,
        probe: &Probe,
        keys: &[&RowKey],
    ) -> Result<Vec<bool>, ExecError> {
        let (table, index) = self.target(txn, probe).await?;
        let stored = keys
            .iter()
            .map(|k| stored_key(k))
            .collect::<Result<Vec<_>, _>>()?;
        let found = rows::probe_prefixes(txn, &table, index.as_ref(), &stored, 1).await?;
        Ok(found.iter().map(|rows| !rows.is_empty()).collect())
    }

    /// Return the outgoing keys among `keys` whose referenced rows don't exist.
    async fn missing(
        &mut self,
        txn: &Txn,
        keys: Vec<(usize, RowKey)>,
    ) -> Result<Vec<(usize, RowKey)>, ExecError> {
        let plan = self.plan.clone();
        let mut out = Vec::new();
        for (i, fk) in plan.outgoing.iter().enumerate() {
            let group: Vec<&RowKey> = keys
                .iter()
                .filter(|(f, _)| *f == i)
                .map(|(_, k)| k)
                .collect();
            if group.is_empty() {
                continue;
            }
            let found = self.exists(txn, &fk.referenced, &group).await?;
            out.extend(
                group
                    .into_iter()
                    .zip(found)
                    .filter(|(_, f)| !f)
                    .map(|(k, _)| (i, k.clone())),
            );
        }
        Ok(out)
    }

    /// Return the incoming keys among `keys` that rows still refer to.
    ///
    /// With `own`, a key the statement wrote again to the target is satisfied.
    async fn dangling(
        &mut self,
        txn: &Txn,
        keys: Vec<(usize, RowKey)>,
        own: bool,
    ) -> Result<Vec<(usize, RowKey)>, ExecError> {
        let plan = self.plan.clone();
        let mut out = Vec::new();
        for (i, fk) in plan.incoming.iter().enumerate() {
            let group: Vec<&RowKey> = keys
                .iter()
                .filter(|(f, _)| *f == i)
                .map(|(_, k)| k)
                .collect();
            if group.is_empty() {
                continue;
            }
            let referred = self.referred(txn, &fk.referencing, &group).await?;
            let mut left: Vec<&RowKey> = group
                .into_iter()
                .zip(referred)
                .filter(|(_, r)| *r)
                .map(|(k, _)| k)
                .collect();
            if own && !left.is_empty() {
                // The referencing key's values, reordered into the target's own key order.
                let reordered: Vec<RowKey> = left
                    .iter()
                    .map(|k| {
                        RowKey(
                            fk.own
                                .key
                                .iter()
                                .map(|p| {
                                    let j = fk
                                        .referencing
                                        .key
                                        .iter()
                                        .position(|q| q == p)
                                        .expect("same columns");
                                    k.0[j].clone()
                                })
                                .collect(),
                        )
                    })
                    .collect();
                let refs: Vec<&RowKey> = reordered.iter().collect();
                let exists = self.exists(txn, &fk.own, &refs).await?;
                left = left
                    .into_iter()
                    .zip(exists)
                    .filter(|(_, e)| !e)
                    .map(|(k, _)| k)
                    .collect();
            }
            out.extend(left.into_iter().map(|k| (i, k.clone())));
        }
        Ok(out)
    }

    /// Probe the keys this batch recorded, keeping those that fail for the end.
    ///
    /// # Errors
    ///
    /// Returns a storage or memory error.
    pub(crate) async fn check_batch(&mut self, txn: &Txn) -> Result<(), ExecError> {
        let (new_keys, new_bytes) = self.new_keys.take();
        let missing = self.missing(txn, new_keys).await?;
        let (removed_keys, removed_bytes) = self.removed_keys.take();
        let dangling = self.dangling(txn, removed_keys, false).await?;
        let mut kept = 0;
        for (i, key) in missing {
            kept += self.pending_new.add(i, key.0).unwrap_or(0);
        }
        for (i, key) in dangling {
            kept += self.pending_removed.add(i, key.0).unwrap_or(0);
        }
        self.memory.shrink(new_bytes + removed_bytes);
        self.memory.grow(kept)?;
        Ok(())
    }

    /// Recheck every kept key once the statement has written all its rows.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::ForeignKey`] for the first key still violated, or a storage error.
    pub(crate) async fn finish(&mut self, txn: &Txn) -> Result<(), ExecError> {
        self.check_batch(txn).await?;
        let plan = self.plan.clone();
        let (pending, bytes) = self.pending_new.take();
        let missing = self.missing(txn, pending).await?;
        let (pending_removed, removed_bytes) = self.pending_removed.take();
        self.memory.shrink(bytes + removed_bytes);
        if let Some((i, key)) = missing.first() {
            let fk = &plan.outgoing[*i];
            return Err(ExecError::ForeignKey(format!(
                "foreign key {}: a row of {} refers to a row of {} with {}, which doesn't exist",
                fk.name,
                plan.target,
                fk.referenced.table,
                shown(&fk.referenced.names, key)
            )));
        }
        let dangling = self.dangling(txn, pending_removed, true).await?;
        if let Some((i, key)) = dangling.first() {
            let fk = &plan.incoming[*i];
            let pending = match fk.on_delete {
                ReferentialAction::NoAction => "",
                ReferentialAction::Cascade => " (on delete cascade isn't carried out yet)",
                ReferentialAction::SetNull => " (on delete set-null isn't carried out yet)",
            };
            return Err(ExecError::ForeignKey(format!(
                "foreign key {}: rows of {} still refer to the row of {} with {}{pending}",
                fk.name,
                fk.referencing.table,
                plan.target,
                shown(&fk.referencing.names, key)
            )));
        }
        Ok(())
    }
}
