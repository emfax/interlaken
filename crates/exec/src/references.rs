//! Foreign key checks for DML (ADR-0035).
//!
//! When a statement writes a table that foreign keys involve, the physical
//! planner attaches a [`ReferencePlan`] to the DML operator, which checks
//! each batch as it is written:
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
//! statement, or a cascade, may insert the missing row or delete the
//! referring rows. Such keys are kept, charged to the query's memory
//! budget, and rechecked when the statement's last DML operator finishes.
//! Only then does a violation fail the statement, with SQLSTATE `23503`.
//! In the usual case every probe succeeds at once and nothing is kept.
//!
//! **Cascades.** A removed key that rows still refer to through a
//! `Cascade` or `SetNull` foreign key is queued instead. When the
//! statement's last DML operator finishes, it runs the queued cascades:
//! for each foreign key, the prepared cascade statement
//! ([`CascadePlans`]), an ordinary delete or update of the referencing
//! table, secured like any statement, with the removed keys as its
//! parameter. Its own DML operator checks its writes and queues further
//! cascades, so the queue is a work list that ends when no cascade removes
//! anything more; each row is changed at most once. A cascade must change
//! every row that refers to the removed keys: it counts them first with a
//! probe that ignores policies, and fails the statement with `42501` if
//! the principal's policies kept it from changing some. The kept keys are
//! rechecked after the last cascade.
//!
//! Every probe registers what it read as a dependency, so a concurrent
//! write that would break a checked reference fails one of the two
//! transactions at every isolation level. Probes read rows the writer's
//! policies may hide: a constraint can't depend on who is asking.
//!

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use interlaken_ir::catalog::{Catalog, ReferentialAction, TableDef, referencing_index};
use interlaken_ir::interp::{Datum, RowKey};
use interlaken_ir::{ColumnId, Ident};
use interlaken_storage::Txn;
use interlaken_storage::catalog::{IndexDef as StoredIndex, TableDef as StoredTable};
use interlaken_storage::encoding::Datum as Stored;
use interlaken_storage::rows;

use crate::error::ExecError;
use crate::exec::ExecContext;
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
    /// The referencing table's name.
    table: String,
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
                table: table.name.joined(),
                columns: fk.columns.iter().map(|&c| at(c)).collect(),
                referenced: Probe {
                    table: referenced.name.joined(),
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
                    table: referencing.name.joined(),
                    index: index.as_ref().map(ToString::to_string),
                    names: sources.iter().map(|&p| column_name(table, p)).collect(),
                    key: sources,
                },
                own: Probe {
                    table: table.name.joined(),
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
            target: table.name.joined(),
            outgoing,
            incoming,
        }))
    }

    /// Return the names and actions of the foreign keys to the target whose
    /// deletes cascade or set NULL, for `EXPLAIN`.
    #[must_use]
    pub fn cascades(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .incoming
            .iter()
            .filter(|fk| fk.on_delete != ReferentialAction::NoAction)
            .map(|fk| format!("{} ({})", fk.name, fk.on_delete.name()))
            .collect();
        out.sort();
        out
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

/// The prepared statement of each cascading foreign key, by name, that a
/// statement's deletes can set off (ADR-0035). Each takes the removed keys
/// as its one parameter (see [`interlaken_ir::catalog::cascade_plan`]).
pub type CascadePlans = HashMap<String, Arc<crate::physical::Physical>>;

/// Build, without security, the cascade of every foreign key `plan`'s
/// deletes can set off, for running plans outside the engine.
///
/// Each cascade statement ([`interlaken_ir::catalog::cascade_plan`]) is
/// resolved, optimized and planned as it is. The engine instead secures
/// each one for the statement's principal, so policies decide which rows
/// a cascade may change.
///
/// # Errors
///
/// Returns [`ExecError::Catalog`] if a cascade statement doesn't resolve,
/// or the planner's error.
pub fn cascade_plans(
    plan: &interlaken_ir::Plan,
    catalog: &dyn Catalog,
) -> Result<CascadePlans, ExecError> {
    let mut out = CascadePlans::new();
    for fk in interlaken_ir::catalog::reachable_cascades(catalog, &plan.root) {
        let Some(cascade) = interlaken_ir::catalog::cascade_plan(catalog, &fk) else {
            continue;
        };
        let resolved = interlaken_ir::resolve::resolve(&cascade, catalog)
            .map_err(|e| ExecError::Catalog(format!("cascade of foreign key {}: {e}", fk.name)))?
            .plan;
        let optimized = interlaken_ir::optimize::optimize(&resolved, catalog);
        out.insert(
            fk.name.to_string(),
            Arc::new(crate::physical::plan(&optimized, catalog)?),
        );
    }
    Ok(out)
}

/// Keys awaiting a check, each once, in the order they were found.
#[derive(Default)]
struct Keys {
    /// Each `(foreign key, key values)`.
    order: Vec<(String, RowKey)>,
    /// The same, for deduplication.
    seen: HashSet<(String, RowKey)>,
}

/// Return the memory one kept key holds.
fn key_memory(fk: &str, key: &RowKey) -> usize {
    2 * (key_bytes(key) + fk.len() + std::mem::size_of::<(String, RowKey)>())
}

impl Keys {
    /// Add `key` of foreign key `fk`; return its memory if it was new.
    fn add(&mut self, fk: &str, key: RowKey) -> Option<usize> {
        let entry = (fk.to_owned(), key);
        if self.seen.contains(&entry) {
            return None;
        }
        let bytes = key_memory(fk, &entry.1);
        self.seen.insert(entry.clone());
        self.order.push(entry);
        Some(bytes)
    }

    /// Remove and return every key, with the memory they held.
    fn take(&mut self) -> (Vec<(String, RowKey)>, usize) {
        let bytes = self.order.iter().map(|(f, k)| key_memory(f, k)).sum();
        self.seen.clear();
        (std::mem::take(&mut self.order), bytes)
    }
}

/// A removed key that rows still refer to through a cascading foreign key.
#[derive(Clone, Debug)]
struct Queued {
    /// The foreign key.
    fk: String,
    /// The key's values in the foreign key's column order: the cascade's parameter.
    key: RowKey,
    /// The same values in the referencing index's order, for probes.
    probe: RowKey,
}

/// The shared state of a statement's foreign key checks.
#[derive(Default)]
struct State {
    /// Foreign keys from tables the statement writes, by name.
    outgoing: HashMap<String, Outgoing>,
    /// Foreign keys to tables the statement writes, by name.
    incoming: HashMap<String, Incoming>,
    /// Stored tables, looked up on first use.
    stored: HashMap<String, StoredTable>,
    /// Outgoing keys that failed their probe.
    pending_new: Keys,
    /// Incoming keys that failed their probe, under `NoAction`.
    pending_removed: Keys,
    /// Cascades to run.
    queued: Vec<Queued>,
    /// The queued `(foreign key, key)` pairs, for deduplication.
    queued_seen: HashSet<(String, RowKey)>,
    /// DML operators of the statement that haven't finished.
    active: usize,
    /// The memory the kept keys hold.
    memory: Option<Reservation>,
}

/// The foreign key checks of one statement, shared by its DML operators
/// and the cascades they set off.
#[derive(Default)]
pub(crate) struct Constraints {
    /// The state, locked only between storage calls.
    state: Mutex<State>,
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

/// The keys one batch of one DML operator recorded.
#[derive(Default)]
pub(crate) struct Recorded {
    /// New referencing keys: `(foreign key, referenced key values)`.
    new: Vec<(String, RowKey)>,
    /// Removed referenced keys.
    removed: Vec<Queued>,
}

impl ReferencePlan {
    /// Record a written row into `out`: `old` is the row before (none for
    /// an insert), `new` the row after (none for a delete).
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::ForeignKey`] for a new row whose referencing
    /// columns are partly NULL under a foreign key that matches FULL.
    pub(crate) fn record(
        &self,
        old: Option<&[Datum]>,
        new: Option<&[Datum]>,
        out: &mut Recorded,
    ) -> Result<(), ExecError> {
        if let Some(new) = new {
            for fk in &self.outgoing {
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
                        self.target, fk.name
                    )));
                }
                out.new
                    .push((fk.name.clone(), RowKey(values(new, &fk.referenced.key))));
            }
        }
        if let Some(old) = old {
            for fk in &self.incoming {
                let referenced = values(old, &fk.columns);
                if new.is_some_and(|new| values(new, &fk.columns) == referenced) {
                    continue;
                }
                // A key with a NULL can't be referred to under either match rule.
                if referenced.iter().any(Datum::is_null) {
                    continue;
                }
                out.removed.push(Queued {
                    fk: fk.name.clone(),
                    key: RowKey(referenced),
                    probe: RowKey(values(old, &fk.referencing.key)),
                });
            }
        }
        Ok(())
    }
}

impl Constraints {
    /// Lock the state.
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register a DML operator of the statement, with the foreign keys it
    /// checks, if any; `memory` is called once, for the first.
    pub(crate) fn enter(&self, plan: Option<&ReferencePlan>, memory: impl FnOnce() -> Reservation) {
        let mut state = self.state();
        state.active += 1;
        if state.memory.is_none() {
            state.memory = Some(memory());
        }
        if let Some(plan) = plan {
            for fk in &plan.outgoing {
                state
                    .outgoing
                    .entry(fk.name.clone())
                    .or_insert_with(|| fk.clone());
            }
            for fk in &plan.incoming {
                state
                    .incoming
                    .entry(fk.name.clone())
                    .or_insert_with(|| fk.clone());
            }
        }
    }

    /// Charge `grow` bytes and release `shrink` bytes of kept keys.
    fn charge(&self, grow: usize, shrink: usize) -> Result<(), ExecError> {
        let mut state = self.state();
        if let Some(memory) = &mut state.memory {
            memory.shrink(shrink);
            memory.grow(grow)?;
        }
        Ok(())
    }

    /// Return the stored definition of table `name`.
    async fn stored(&self, txn: &Txn, name: &str) -> Result<StoredTable, ExecError> {
        if let Some(table) = self.state().stored.get(name) {
            return Ok(table.clone());
        }
        let table = interlaken_storage::catalog::table(txn, name)
            .await?
            .ok_or_else(|| ExecError::Catalog(format!("table {name} is not stored")))?;
        self.state().stored.insert(name.to_owned(), table.clone());
        Ok(table)
    }

    /// Return the stored table `probe` reads, and its index if it reads one.
    async fn target(
        &self,
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
        &self,
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

    /// Return, for each of `keys`, how many rows' `probe` key starts with
    /// it, counting at most `limit`.
    async fn referring(
        &self,
        txn: &Txn,
        probe: &Probe,
        keys: &[&RowKey],
        limit: usize,
    ) -> Result<Vec<usize>, ExecError> {
        let (table, index) = self.target(txn, probe).await?;
        let stored = keys
            .iter()
            .map(|k| stored_key(k))
            .collect::<Result<Vec<_>, _>>()?;
        let found = rows::probe_prefixes(txn, &table, index.as_ref(), &stored, limit).await?;
        Ok(found.iter().map(Vec::len).collect())
    }

    /// Return the outgoing keys among `keys` whose referenced rows don't exist.
    async fn missing(
        &self,
        txn: &Txn,
        keys: Vec<(String, RowKey)>,
    ) -> Result<Vec<(String, RowKey)>, ExecError> {
        let mut groups: Vec<(String, Vec<RowKey>)> = Vec::new();
        for (fk, key) in keys {
            match groups.iter_mut().find(|(f, _)| *f == fk) {
                Some((_, group)) => group.push(key),
                None => groups.push((fk, vec![key])),
            }
        }
        let mut out = Vec::new();
        for (fk, group) in groups {
            let probe = self.state().outgoing[&fk].referenced.clone();
            let refs: Vec<&RowKey> = group.iter().collect();
            let found = self.exists(txn, &probe, &refs).await?;
            out.extend(
                group
                    .into_iter()
                    .zip(found)
                    .filter(|(_, f)| !f)
                    .map(|(k, _)| (fk.clone(), k)),
            );
        }
        Ok(out)
    }

    /// Return the removed keys among `keys` (in referencing index order)
    /// that rows still refer to. With `own`, a key the statement wrote
    /// again to the referenced table is satisfied.
    async fn dangling(
        &self,
        txn: &Txn,
        keys: Vec<(String, RowKey)>,
        own: bool,
    ) -> Result<Vec<(String, RowKey)>, ExecError> {
        let mut groups: Vec<(String, Vec<RowKey>)> = Vec::new();
        for (fk, key) in keys {
            match groups.iter_mut().find(|(f, _)| *f == fk) {
                Some((_, group)) => group.push(key),
                None => groups.push((fk, vec![key])),
            }
        }
        let mut out = Vec::new();
        for (name, group) in groups {
            let fk = self.state().incoming[&name].clone();
            let refs: Vec<&RowKey> = group.iter().collect();
            let referring = self.referring(txn, &fk.referencing, &refs, 1).await?;
            let mut left: Vec<RowKey> = group
                .into_iter()
                .zip(referring)
                .filter(|(_, n)| *n > 0)
                .map(|(k, _)| k)
                .collect();
            if own && !left.is_empty() {
                // The referencing key's values, reordered into the referenced key's order.
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
            out.extend(left.into_iter().map(|k| (name.clone(), k)));
        }
        Ok(out)
    }

    /// Probe one batch's recorded keys, keeping those that fail for the end
    /// and queueing cascades.
    ///
    /// # Errors
    ///
    /// Returns a storage or memory error.
    pub(crate) async fn check_batch(&self, txn: &Txn, recorded: Recorded) -> Result<(), ExecError> {
        let Recorded { new, removed } = recorded;
        let missing = self.missing(txn, new).await?;
        let probes: Vec<(String, RowKey)> = removed
            .iter()
            .map(|q| (q.fk.clone(), q.probe.clone()))
            .collect();
        let dangling = self.dangling(txn, probes, false).await?;
        let mut kept = 0;
        let mut state = self.state();
        for (fk, key) in missing {
            kept += state.pending_new.add(&fk, key).unwrap_or(0);
        }
        for (fk, probe) in dangling {
            let action = state.incoming[&fk].on_delete;
            if action == ReferentialAction::NoAction {
                kept += state.pending_removed.add(&fk, probe).unwrap_or(0);
                continue;
            }
            let queued = removed
                .iter()
                .find(|q| q.fk == fk && q.probe == probe)
                .expect("dangling keys were recorded")
                .clone();
            if state.queued_seen.insert((fk.clone(), queued.key.clone())) {
                kept += key_memory(&fk, &queued.key);
                state.queued.push(queued);
            }
        }
        drop(state);
        self.charge(kept, 0)
    }

    /// Finish one DML operator of the statement. The last to finish runs
    /// the queued cascades, then rechecks every kept key.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::ForeignKey`] for the first key still violated,
    /// [`ExecError::PermissionDenied`] if policies kept a cascade from
    /// changing every row it had to, or a storage error.
    pub(crate) async fn finish(&self, ctx: &ExecContext) -> Result<(), ExecError> {
        if self.state().active > 1 {
            self.state().active -= 1;
            return Ok(());
        }
        self.cascade(ctx).await?;
        self.recheck(&ctx.txn).await?;
        self.state().active -= 1;
        Ok(())
    }

    /// Run queued cascades until none is left.
    async fn cascade(&self, ctx: &ExecContext) -> Result<(), ExecError> {
        loop {
            let queued = std::mem::take(&mut self.state().queued);
            if queued.is_empty() {
                return Ok(());
            }
            let mut groups: Vec<(String, Vec<Queued>)> = Vec::new();
            for q in queued {
                match groups.iter_mut().find(|(f, _)| *f == q.fk) {
                    Some((_, group)) => group.push(q),
                    None => groups.push((q.fk.clone(), vec![q])),
                }
            }
            for (name, group) in groups {
                let fk = self.state().incoming[&name].clone();
                let plan = ctx
                    .cascades
                    .as_ref()
                    .and_then(|plans| plans.get(&name))
                    .cloned()
                    .ok_or_else(|| {
                        ExecError::Unsupported(format!(
                            "foreign key {name}'s on delete {} without a prepared cascade",
                            fk.on_delete.name()
                        ))
                    })?;
                // Every row referring to the keys must change.
                let probes: Vec<&RowKey> = group.iter().map(|q| &q.probe).collect();
                let referring: usize = self
                    .referring(&ctx.txn, &fk.referencing, &probes, usize::MAX)
                    .await?
                    .into_iter()
                    .sum();
                // The cascade reads everything the statement has written so far.
                ctx.txn.next_statement();
                let keys = Datum::Array(
                    group
                        .iter()
                        .map(|q| Datum::Record(q.key.0.clone()))
                        .collect(),
                );
                let mut cascade = ctx.clone();
                cascade.params = vec![keys];
                cascade.outer = Arc::new([]);
                let changed: usize = crate::exec::collect(&plan, &cascade)
                    .await?
                    .iter()
                    .map(arrow_array::RecordBatch::num_rows)
                    .sum();
                let released: usize = group.iter().map(|q| key_memory(&q.fk, &q.key)).sum();
                {
                    let mut state = self.state();
                    for q in &group {
                        state.queued_seen.remove(&(q.fk.clone(), q.key.clone()));
                    }
                }
                self.charge(0, released)?;
                if changed < referring {
                    return Err(ExecError::PermissionDenied {
                        table: fk.referencing.table.clone(),
                    });
                }
            }
        }
    }

    /// Recheck every kept key.
    async fn recheck(&self, txn: &Txn) -> Result<(), ExecError> {
        let (pending, bytes) = self.state().pending_new.take();
        let missing = self.missing(txn, pending).await?;
        let (pending_removed, removed_bytes) = self.state().pending_removed.take();
        self.charge(0, bytes + removed_bytes)?;
        if let Some((name, key)) = missing.first() {
            let state = self.state();
            let fk = &state.outgoing[name];
            return Err(ExecError::ForeignKey(format!(
                "foreign key {}: a row of {} refers to a row of {} with {}, which doesn't exist",
                fk.name,
                fk.table,
                fk.referenced.table,
                shown(&fk.referenced.names, key)
            )));
        }
        let dangling = self.dangling(txn, pending_removed, true).await?;
        if let Some((name, key)) = dangling.first() {
            let state = self.state();
            let fk = &state.incoming[name];
            return Err(ExecError::ForeignKey(format!(
                "foreign key {}: rows of {} still refer to the row of {} with {}",
                fk.name,
                fk.referencing.table,
                fk.own.table,
                shown(&fk.referencing.names, key)
            )));
        }
        Ok(())
    }
}
