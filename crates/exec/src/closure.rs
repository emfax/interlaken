//! Closure maintenance for DML on membership edges tables (ADR-0017, ADR-0032).
//!
//! A membership's closure table holds, for every pair of entities connected
//! by membership edges, the number of distinct paths between them, as
//! `security::Closure` does in memory. When a statement inserts, deletes or
//! updates rows of an edges table, the physical planner attaches a
//! [`ClosurePlan`] to the DML operator, and a [`Maintainer`] adjusts the
//! path counts of every affected pair as each batch is written:
//!
//! - adding `m -> g` adds `paths(x, m) * paths(g, y)` to `(x, y)` for every
//!   `x` reaching `m` (including `m`) and every `y` reachable from `g`
//!   (including `g`); removing it subtracts the same, and a pair whose
//!   count reaches zero is deleted;
//! - an update is a removal of the old edge and an addition of the new one;
//! - an edge that would close a cycle fails the statement (SQLSTATE `23514`).
//!
//! Hierarchies can span entity types (documents in folders in folders), so
//! the entities reaching `m` are read from every closure table whose
//! ancestor type is `m`'s, and likewise above `g`. Each pair is written to
//! the closure table of its two types, so the planner requires the declared
//! memberships to be closed under composition for the types involved.
//!
//! Storage reads hide the current statement's writes (ADR-0026), so the
//! maintainer keeps the statement's counts in memory and writes each
//! changed pair once per batch. Every closure range it reads is registered
//! as a dependency, so a concurrent membership change that would make the
//! counts inconsistent fails one of the transactions at every isolation
//! level.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use interlaken_ir::catalog::{Catalog, TableDef};
use interlaken_ir::interp::Datum;
use interlaken_ir::security::EntityKey;
use interlaken_ir::{Ident, TableId};
use interlaken_storage::Txn;
use interlaken_storage::catalog::TableDef as StoredTable;
use interlaken_storage::encoding::{ColumnType, Datum as Stored};
use interlaken_storage::rows::{self, RowScan};

use crate::error::ExecError;
use crate::memory::Reservation;

/// One closure table and where its columns are.
#[derive(Clone, Debug)]
struct ClosureTable {
    /// The descendant's entity type.
    member: Ident,
    /// The ancestor's entity type.
    group: Ident,
    /// The table.
    table: Arc<TableDef>,
    /// Positions of the descendant, ancestor and path count columns.
    columns: (usize, usize, usize),
}

/// One membership whose edges are the DML target's rows.
#[derive(Clone, Debug)]
struct Hierarchy {
    /// The member entity type.
    member: Ident,
    /// The group entity type.
    group: Ident,
    /// Positions of the member and group key columns in the edges table.
    edge_columns: (usize, usize),
}

/// What a DML statement on an edges table must keep up to date: the
/// memberships whose edges it writes and every closure table they reach.
#[derive(Clone, Debug)]
pub struct ClosurePlan {
    /// The memberships whose edges table is the target.
    hierarchies: Vec<Hierarchy>,
    /// The closure tables an edge change can touch.
    closures: Vec<ClosureTable>,
    /// Closure table index by (descendant type, ancestor type).
    by_types: BTreeMap<(Ident, Ident), usize>,
}

/// Return a column ID as a position.
fn position(column: interlaken_ir::ColumnId) -> usize {
    usize::try_from(column.get()).expect("column positions fit in usize")
}

/// Return the entity types with a membership path to `start` (`up` false)
/// or from it (`up` true), excluding `start` unless a cycle of types reaches it.
fn reach(edges: &BTreeSet<(Ident, Ident)>, start: &Ident, up: bool) -> BTreeSet<Ident> {
    let mut seen = BTreeSet::new();
    let mut frontier = vec![start.clone()];
    while let Some(ty) = frontier.pop() {
        for (member, group) in edges {
            let (from, to) = if up { (member, group) } else { (group, member) };
            if *from == ty && seen.insert(to.clone()) {
                frontier.push(to.clone());
            }
        }
    }
    seen
}

impl ClosurePlan {
    /// Return the closure plan for DML on `table`, or `None` if no
    /// membership has it as its edges table.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Unsupported`] if a closure table the edges can
    /// affect is not declared (the memberships of the types involved must
    /// be closed under composition), or a closure table's primary key is
    /// not its descendant and ancestor columns; [`ExecError::Catalog`] if
    /// a declared table is missing.
    pub fn for_table(catalog: &dyn Catalog, table: TableId) -> Result<Option<Self>, ExecError> {
        let memberships: Vec<_> = catalog
            .entities()
            .iter()
            .flat_map(|e| catalog.memberships_of(&e.name))
            .collect();
        let hierarchies: Vec<Hierarchy> = memberships
            .iter()
            .filter(|m| m.edges == table)
            .map(|m| Hierarchy {
                member: m.member.clone(),
                group: m.group.clone(),
                edge_columns: (position(m.edge_columns.0), position(m.edge_columns.1)),
            })
            .collect();
        if hierarchies.is_empty() {
            return Ok(None);
        }
        let type_edges: BTreeSet<(Ident, Ident)> = memberships
            .iter()
            .map(|m| (m.member.clone(), m.group.clone()))
            .collect();
        let mut plan = Self {
            hierarchies: Vec::new(),
            closures: Vec::new(),
            by_types: BTreeMap::new(),
        };
        for h in &hierarchies {
            let below = reach(&type_edges, &h.member, false);
            let above = reach(&type_edges, &h.group, true);
            let mut needed: BTreeSet<(Ident, Ident)> = BTreeSet::new();
            needed.extend(below.iter().map(|t| (t.clone(), h.member.clone())));
            needed.extend(above.iter().map(|a| (h.group.clone(), a.clone())));
            for t in below.iter().chain(std::iter::once(&h.member)) {
                for a in above.iter().chain(std::iter::once(&h.group)) {
                    needed.insert((t.clone(), a.clone()));
                }
            }
            if type_edges.contains(&(h.group.clone(), h.member.clone())) {
                needed.insert((h.group.clone(), h.member.clone()));
            }
            for (member, group) in needed {
                plan.closure(catalog, &member, &group)?;
            }
        }
        plan.hierarchies = hierarchies;
        Ok(Some(plan))
    }

    /// Register the closure table of `member` in `group`, which must be declared.
    fn closure(
        &mut self,
        catalog: &dyn Catalog,
        member: &Ident,
        group: &Ident,
    ) -> Result<(), ExecError> {
        let key = (member.clone(), group.clone());
        if self.by_types.contains_key(&key) {
            return Ok(());
        }
        let def = catalog.membership(member, group).ok_or_else(|| {
            ExecError::Unsupported(format!(
                "maintaining closures needs a membership of {member} in {group}, \
                 since edge changes connect them"
            ))
        })?;
        let table = catalog.table_by_id(def.closure).ok_or_else(|| {
            ExecError::Catalog(format!("closure table of {member} in {group} is missing"))
        })?;
        let (d, a, p) = def.closure_columns;
        let mut key_columns: Vec<_> = table.primary_key.clone();
        key_columns.sort_unstable();
        let mut expected = vec![d, a];
        expected.sort_unstable();
        if key_columns != expected {
            return Err(ExecError::Unsupported(format!(
                "closure table {} must have its descendant and ancestor columns as its primary key",
                table.name
            )));
        }
        self.by_types.insert(key, self.closures.len());
        self.closures.push(ClosureTable {
            member: member.clone(),
            group: group.clone(),
            table,
            columns: (position(d), position(a), position(p)),
        });
        Ok(())
    }

    /// Return the names of the closure tables the plan writes, for `EXPLAIN`.
    #[must_use]
    pub fn table_names(&self) -> Vec<String> {
        self.closures
            .iter()
            .map(|c| c.table.name.joined())
            .collect()
    }

    /// Return the closure table of `member` in `group`, if the plan has one.
    fn index(&self, member: &Ident, group: &Ident) -> Option<usize> {
        self.by_types.get(&(member.clone(), group.clone())).copied()
    }
}

/// Return the closure plan for DML on `table`, refusing DML on a closure
/// table: only the engine maintains those.
///
/// # Errors
///
/// Returns [`ExecError::PermissionDenied`] for a closure table, or the
/// errors of [`ClosurePlan::for_table`].
pub(crate) fn guard_dml(
    catalog: &dyn Catalog,
    table: &TableDef,
) -> Result<Option<Arc<ClosurePlan>>, ExecError> {
    let maintained = catalog
        .entities()
        .iter()
        .flat_map(|e| catalog.memberships_of(&e.name))
        .any(|m| m.closure == table.id);
    if maintained {
        return Err(ExecError::PermissionDenied {
            table: table.name.joined(),
        });
    }
    Ok(ClosurePlan::for_table(catalog, table.id)?.map(Arc::new))
}

/// Fail if more than one DML operator in `root` maintains the same closure
/// table: each keeps its own counts, so they would overwrite each other's.
///
/// # Errors
///
/// Returns [`ExecError::Unsupported`] for such a plan.
pub(crate) fn check_single_maintainer(root: &crate::physical::Physical) -> Result<(), ExecError> {
    /// Collect the closure tables each DML operator under `node` maintains.
    fn collect(node: &crate::physical::Physical, out: &mut Vec<TableId>) -> Result<(), ExecError> {
        use crate::physical::Op;
        if let Op::Insert { dml, .. } | Op::Update { dml, .. } | Op::Delete { dml, .. } = &node.op
            && let Some(plan) = &dml.closure
        {
            for c in &plan.closures {
                if out.contains(&c.table.id) {
                    return Err(ExecError::Unsupported(format!(
                        "two writes in one statement maintaining closure table {}",
                        c.table.name
                    )));
                }
                out.push(c.table.id);
            }
        }
        node.op
            .inputs()
            .into_iter()
            .try_for_each(|i| collect(i, out))
    }
    collect(root, &mut Vec::new())
}

/// One edge change, in statement order.
#[derive(Clone, Debug)]
struct Change {
    /// Whether the edge is added (or removed).
    add: bool,
    /// The hierarchy.
    hierarchy: usize,
    /// The member's key.
    member: EntityKey,
    /// The group's key.
    group: EntityKey,
}

/// A pair's path count when the statement first read it, and now.
#[derive(Clone, Copy, Debug)]
struct Count {
    /// The count in storage (as of the last flush).
    stored: u64,
    /// The count after the statement's changes so far.
    current: u64,
}

/// Which column of a closure table a lookup matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// The descendant column.
    Descendant,
    /// The ancestor column.
    Ancestor,
}

/// Maintains the closure tables during one DML statement.
pub(crate) struct Maintainer {
    /// What to maintain.
    plan: Arc<ClosurePlan>,
    /// The stored closure tables, looked up on first use.
    stored: Vec<Option<StoredTable>>,
    /// Path counts of the pairs the statement has read or changed.
    counts: BTreeMap<(usize, EntityKey, EntityKey), Count>,
    /// Edge changes not applied yet.
    pending: Vec<Change>,
    /// The memory held by `counts`.
    memory: Reservation,
}

/// Convert a key column's value to an entity key; `None` for NULL.
fn entity_key(d: &Datum) -> Result<Option<EntityKey>, ExecError> {
    Ok(Some(match d {
        Datum::Null => return Ok(None),
        Datum::Int2(v) => EntityKey::Long(i64::from(*v)),
        Datum::Int4(v) => EntityKey::Long(i64::from(*v)),
        Datum::Int8(v) => EntityKey::Long(*v),
        Datum::Text(v) => EntityKey::Str(v.clone()),
        other => {
            return Err(ExecError::Unsupported(format!(
                "membership key {other:?}: keys are integers or text"
            )));
        }
    }))
}

/// Convert a stored key column's value to an entity key.
fn stored_key(d: &Stored) -> Result<EntityKey, ExecError> {
    entity_key(&crate::operators::from_stored(d))?
        .ok_or_else(|| ExecError::Catalog("NULL key in a closure table".into()))
}

/// Return `key` as a stored value of column type `ty`.
fn stored_value(key: &EntityKey, ty: ColumnType) -> Result<Stored, ExecError> {
    let out_of_range = || ExecError::Unsupported(format!("key {key:?} in a {ty:?} column"));
    Ok(match (key, ty) {
        (EntityKey::Long(v), ColumnType::Int2) => {
            Stored::Int2(i16::try_from(*v).map_err(|_| out_of_range())?)
        }
        (EntityKey::Long(v), ColumnType::Int4) => {
            Stored::Int4(i32::try_from(*v).map_err(|_| out_of_range())?)
        }
        (EntityKey::Long(v), ColumnType::Int8) => Stored::Int8(*v),
        (EntityKey::Str(v), ColumnType::Text) => Stored::Text(v.clone()),
        _ => return Err(out_of_range()),
    })
}

/// Return a path count as a stored value of column type `ty`.
fn stored_count(n: u64, ty: ColumnType) -> Result<Stored, ExecError> {
    let key = EntityKey::Long(
        i64::try_from(n).map_err(|_| ExecError::Unsupported("path count overflow".into()))?,
    );
    stored_value(&key, ty)
}

/// Read a stored path count.
fn count_of(d: &Stored) -> Result<u64, ExecError> {
    match stored_key(d)? {
        EntityKey::Long(n) if n > 0 => Ok(n.cast_unsigned()),
        _ => Err(ExecError::Catalog(
            "invalid path count in a closure table".into(),
        )),
    }
}

impl Maintainer {
    /// Construct a maintainer for one statement, charging its memory to `memory`.
    pub(crate) fn new(plan: Arc<ClosurePlan>, memory: Reservation) -> Self {
        let stored = vec![None; plan.closures.len()];
        Self {
            plan,
            stored,
            counts: BTreeMap::new(),
            pending: Vec::new(),
            memory,
        }
    }

    /// Record that a row of the edges table changed from `old` to `new`
    /// (`None` for an insert's old row or a delete's new row).
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Unsupported`] for a key that is neither an integer nor text.
    pub(crate) fn record(
        &mut self,
        old: Option<&[Datum]>,
        new: Option<&[Datum]>,
    ) -> Result<(), ExecError> {
        for (i, h) in self.plan.hierarchies.iter().enumerate() {
            let edge =
                |row: Option<&[Datum]>| -> Result<Option<(EntityKey, EntityKey)>, ExecError> {
                    let Some(row) = row else { return Ok(None) };
                    Ok(
                        entity_key(&row[h.edge_columns.0])?
                            .zip(entity_key(&row[h.edge_columns.1])?),
                    )
                };
            let (before, after) = (edge(old)?, edge(new)?);
            if before == after {
                continue;
            }
            for (add, edge) in [(false, before), (true, after)] {
                if let Some((member, group)) = edge {
                    self.pending.push(Change {
                        add,
                        hierarchy: i,
                        member,
                        group,
                    });
                }
            }
        }
        Ok(())
    }

    /// Apply the recorded changes and write the changed path counts.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::MembershipCycle`] if an added edge would make a
    /// hierarchy cyclic, [`ExecError::Catalog`] if a closure table is
    /// inconsistent with its edges, or a storage error.
    pub(crate) async fn apply(&mut self, txn: &Txn) -> Result<(), ExecError> {
        for change in std::mem::take(&mut self.pending) {
            self.change(txn, &change).await?;
        }
        self.flush(txn).await
    }

    /// Return the stored table of closure `ci`, looking it up on first use.
    async fn stored(&mut self, txn: &Txn, ci: usize) -> Result<StoredTable, ExecError> {
        if let Some(t) = &self.stored[ci] {
            return Ok(t.clone());
        }
        let name = self.plan.closures[ci].table.name.joined();
        let table = interlaken_storage::catalog::table(txn, &name)
            .await?
            .ok_or_else(|| ExecError::Catalog(format!("table {name} is not stored")))?;
        self.stored[ci] = Some(table.clone());
        Ok(table)
    }

    /// Return the path count of `(d, a)` in closure `ci`, reading it on first use.
    async fn count(
        &mut self,
        txn: &Txn,
        ci: usize,
        d: &EntityKey,
        a: &EntityKey,
    ) -> Result<u64, ExecError> {
        let key = (ci, d.clone(), a.clone());
        if let Some(c) = self.counts.get(&key) {
            return Ok(c.current);
        }
        let table = self.stored(txn, ci).await?;
        let pk = self.primary_key(&table, ci, d, a)?;
        rows::register_row_dependency(txn, &table, &pk)?;
        let n = match rows::get(txn, &table, &pk).await? {
            Some(row) => count_of(&row[self.plan.closures[ci].columns.2])?,
            None => 0,
        };
        self.remember(key, n)?;
        Ok(n)
    }

    /// Keep a pair's count as read from storage.
    fn remember(&mut self, key: (usize, EntityKey, EntityKey), n: u64) -> Result<(), ExecError> {
        let bytes = std::mem::size_of::<(usize, EntityKey, EntityKey, Count)>()
            + [&key.1, &key.2]
                .iter()
                .map(|k| match k {
                    EntityKey::Str(s) => s.len(),
                    EntityKey::Long(_) => 0,
                })
                .sum::<usize>();
        self.memory.grow(bytes)?;
        self.counts.insert(
            key,
            Count {
                stored: n,
                current: n,
            },
        );
        Ok(())
    }

    /// Return the stored primary key of pair `(d, a)` in closure `ci`.
    fn primary_key(
        &self,
        table: &StoredTable,
        ci: usize,
        d: &EntityKey,
        a: &EntityKey,
    ) -> Result<Vec<Stored>, ExecError> {
        let dc = self.plan.closures[ci].columns.0;
        let types = table.types();
        table
            .primary_key()
            .iter()
            .map(|&c| stored_value(if c == dc { d } else { a }, types[c]))
            .collect()
    }

    /// Return every pair of closure `ci` whose `side` column is `node`, as
    /// (other end, path count), including the statement's changes.
    async fn related(
        &mut self,
        txn: &Txn,
        ci: usize,
        side: Side,
        node: &EntityKey,
    ) -> Result<Vec<(EntityKey, u64)>, ExecError> {
        let table = self.stored(txn, ci).await?;
        let (dc, ac, pc) = self.plan.closures[ci].columns;
        let (column, other) = match side {
            Side::Descendant => (dc, ac),
            Side::Ancestor => (ac, dc),
        };
        let value = stored_value(node, table.types()[column])?;
        let mut found: BTreeMap<EntityKey, u64> = BTreeMap::new();
        let index = table
            .indexes()
            .iter()
            .find(|i| i.columns().first().map(|c| c.column) == Some(column));
        if table.primary_key().first() == Some(&column) || index.is_none() {
            // A primary key prefix scan, or a full scan without a usable index.
            let prefix = if table.primary_key().first() == Some(&column) {
                vec![value.clone()]
            } else {
                Vec::new()
            };
            rows::register_scan_dependency(txn, &table, None, &prefix)?;
            let mut scan = RowScan::prefix(prefix);
            loop {
                let batch = rows::scan(txn, &table, &scan).await?;
                for row in &batch.rows {
                    if row[column] == value {
                        found.insert(stored_key(&row[other])?, count_of(&row[pc])?);
                    }
                }
                match batch.resume {
                    Some(resume) => scan.resume = Some(resume),
                    None => break,
                }
            }
        } else if let Some(index) = index {
            let prefix = vec![value.clone()];
            rows::register_scan_dependency(txn, &table, Some(index), &prefix)?;
            let mut scan = RowScan::prefix(prefix);
            loop {
                let batch = rows::scan_index(txn, &table, index, &scan).await?;
                for row in rows::get_many(txn, &table, &batch.primary_keys)
                    .await?
                    .into_iter()
                    .flatten()
                {
                    found.insert(stored_key(&row[other])?, count_of(&row[pc])?);
                }
                match batch.resume {
                    Some(resume) => scan.resume = Some(resume),
                    None => break,
                }
            }
        }
        // The statement's own counts override what storage shows.
        for ((c, d, a), count) in &self.counts {
            if *c != ci {
                continue;
            }
            let (this, that) = match side {
                Side::Descendant => (d, a),
                Side::Ancestor => (a, d),
            };
            if this == node {
                if count.current == 0 {
                    found.remove(that);
                } else {
                    found.insert(that.clone(), count.current);
                }
            }
        }
        Ok(found.into_iter().collect())
    }

    /// Apply one edge change to the in-memory counts.
    async fn change(&mut self, txn: &Txn, change: &Change) -> Result<(), ExecError> {
        let plan = self.plan.clone();
        let h = &plan.hierarchies[change.hierarchy];
        let (m, g) = (&change.member, &change.group);
        if change.add {
            let cyclic = if h.member == h.group && m == g {
                true
            } else if let Some(ci) = plan.index(&h.group, &h.member) {
                self.count(txn, ci, g, m).await? > 0
            } else {
                false
            };
            if cyclic {
                return Err(ExecError::MembershipCycle {
                    member: format!("{}::{}", h.member, display(m)),
                    group: format!("{}::{}", h.group, display(g)),
                });
            }
        }
        // Entities reaching m, and reachable from g, with path counts.
        let mut reaching = vec![(h.member.clone(), m.clone(), 1u64)];
        let mut reachable = vec![(h.group.clone(), g.clone(), 1u64)];
        for (ci, c) in plan.closures.iter().enumerate() {
            if c.group == h.member {
                for (d, n) in self.related(txn, ci, Side::Ancestor, m).await? {
                    reaching.push((c.member.clone(), d, n));
                }
            }
            if c.member == h.group {
                for (a, n) in self.related(txn, ci, Side::Descendant, g).await? {
                    reachable.push((c.group.clone(), a, n));
                }
            }
        }
        for (t, x, a) in &reaching {
            for (u, y, b) in &reachable {
                let ci = plan
                    .index(t, u)
                    .ok_or_else(|| ExecError::Catalog(format!("no closure table of {t} in {u}")))?;
                let n = self.count(txn, ci, x, y).await?;
                let delta = a
                    .checked_mul(*b)
                    .ok_or_else(|| ExecError::Unsupported("path count overflow".into()))?;
                let next = if change.add {
                    n.checked_add(delta)
                        .ok_or_else(|| ExecError::Unsupported("path count overflow".into()))?
                } else {
                    n.checked_sub(delta).ok_or_else(|| {
                        ExecError::Catalog(format!(
                            "closure table {} is inconsistent with its edges",
                            plan.closures[ci].table.name
                        ))
                    })?
                };
                let entry = self
                    .counts
                    .get_mut(&(ci, x.clone(), y.clone()))
                    .expect("counted above");
                entry.current = next;
            }
        }
        Ok(())
    }

    /// Write every pair whose count changed since the last flush.
    async fn flush(&mut self, txn: &Txn) -> Result<(), ExecError> {
        let changed: Vec<_> = self
            .counts
            .iter()
            .filter(|(_, c)| c.stored != c.current)
            .map(|(k, c)| (k.clone(), *c))
            .collect();
        for ((ci, d, a), count) in changed {
            let table = self.stored(txn, ci).await?;
            let pk = self.primary_key(&table, ci, &d, &a)?;
            let types = table.types();
            let (dc, ac, pc) = self.plan.closures[ci].columns;
            let row = || -> Result<Vec<Stored>, ExecError> {
                let mut row = vec![Stored::Null; types.len()];
                row[dc] = stored_value(&d, types[dc])?;
                row[ac] = stored_value(&a, types[ac])?;
                row[pc] = stored_count(count.current, types[pc])?;
                Ok(row)
            };
            match (count.stored, count.current) {
                (0, _) => rows::insert(txn, &table, row()?).await?,
                (_, 0) => {
                    rows::delete(txn, &table, &pk).await?;
                }
                _ => {
                    rows::update(txn, &table, &pk, row()?).await?;
                }
            }
            if let Some(c) = self.counts.get_mut(&(ci, d, a)) {
                c.stored = c.current;
            }
        }
        Ok(())
    }
}

/// Render a key as in an entity reference.
fn display(key: &EntityKey) -> String {
    match key {
        EntityKey::Long(v) => format!("\"{v}\""),
        EntityKey::Str(v) => format!("{v:?}"),
    }
}
