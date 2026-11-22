//! Evaluating relational operators.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::datum::{Datum, RowKey};
use super::error::EvalError;
use super::eval::{Frame, Layout, truth};
use super::run::{Database, ExecOptions};
use crate::catalog::{Catalog, FunctionDef, TableDef};
use crate::expr::{Expr, NamedExpr};
use crate::names::{BindingId, ColumnId, FunctionId, FunctionRef, TableId, TableRef, TargetColumn};
use crate::rel::{ApplyKind, DmlTarget, JoinKind, Rel, SetOpKind, SortKey};

/// A relation's rows, with the layout of each row.
pub(crate) struct Relation {
    /// What each column of a row is.
    pub(crate) layout: Layout,
    /// The rows.
    pub(crate) rows: Vec<Vec<Datum>>,
}

/// The interpreter's state while running one plan.
pub(crate) struct Interp<'a> {
    /// The catalog, for table and function definitions.
    pub(crate) catalog: &'a dyn Catalog,
    /// The data, changed by DML.
    pub(crate) db: &'a mut Database,
    /// Parameter values, `$1` first.
    pub(crate) params: &'a [Datum],
    /// Settings such as the transaction time.
    pub(crate) options: ExecOptions,
    /// Function definitions looked up so far.
    functions: HashMap<FunctionId, Arc<FunctionDef>>,
    /// State of the random number generator.
    pub(crate) rng: u64,
    /// The table each `Get` binding reads, for finding DML target rows.
    binding_tables: HashMap<BindingId, crate::names::TableId>,
}

/// Return the layout `binding.0 .. binding.(n-1)`.
fn layout_of(binding: Option<BindingId>, n: usize) -> Result<Layout, EvalError> {
    let binding =
        binding.ok_or_else(|| EvalError::Unsupported("operator without a binding".into()))?;
    (0..n)
        .map(|i| {
            u32::try_from(i)
                .map(|c| (binding, ColumnId::new(c)))
                .map_err(|_| EvalError::Unsupported("too many columns".into()))
        })
        .collect()
}

/// Return the frames for a row of `layout` inside `outer`.
fn frames<'a>(
    layout: &'a [(BindingId, ColumnId)],
    row: &'a [Datum],
    outer: &[Frame<'a>],
) -> Vec<Frame<'a>> {
    std::iter::once(Frame { layout, row })
        .chain(outer.iter().copied())
        .collect()
}

/// Remove duplicate rows, keeping the first of each.
fn dedup(rows: Vec<Vec<Datum>>) -> Vec<Vec<Datum>> {
    let mut seen = HashSet::new();
    rows.into_iter()
        .filter(|r| seen.insert(RowKey(r.clone())))
        .collect()
}

/// Count the rows of `rows` by value.
fn counts(rows: &[Vec<Datum>]) -> HashMap<RowKey, usize> {
    let mut counts = HashMap::new();
    for row in rows {
        *counts.entry(RowKey(row.clone())).or_insert(0) += 1;
    }
    counts
}

/// Compare two rows by the values of their sort keys, as `ORDER BY` does.
///
/// `a` and `b` hold one value per key. NULLs go first or last per key, and
/// descending keys reverse the order of non-NULL values only. The executor's
/// `Sort` uses this so both order rows identically.
///
/// # Examples
///
/// ```
/// use std::cmp::Ordering;
/// use interlaken_ir::interp::{Datum, compare_sort_keys};
/// use interlaken_ir::{Expr, SortKey, Value};
///
/// let key = [SortKey::desc(Expr::Literal(Value::Int4(0)))];
/// // Descending keys put NULLs first by default.
/// assert_eq!(compare_sort_keys(&[Datum::Null], &[Datum::Int4(1)], &key), Ordering::Less);
/// assert_eq!(compare_sort_keys(&[Datum::Int4(2)], &[Datum::Int4(1)], &key), Ordering::Less);
/// ```
#[must_use]
pub fn compare_sort_keys(a: &[Datum], b: &[Datum], keys: &[SortKey]) -> Ordering {
    for ((x, y), key) in a.iter().zip(b).zip(keys) {
        let ordering = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let o = x.compare(y).unwrap_or(Ordering::Equal);
                if key.descending { o.reverse() } else { o }
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

impl<'a> Interp<'a> {
    /// Construct an interpreter over `db`.
    pub(crate) fn new(
        catalog: &'a dyn Catalog,
        db: &'a mut Database,
        params: &'a [Datum],
        options: ExecOptions,
    ) -> Self {
        Self {
            catalog,
            db,
            params,
            rng: options.seed | 1,
            options,
            functions: HashMap::new(),
            binding_tables: HashMap::new(),
        }
    }

    /// Return the next pseudo-random number (xorshift64*).
    pub(crate) fn next_random(&mut self) -> u64 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Look up a function definition.
    pub(crate) fn function(&mut self, id: FunctionId) -> Result<Arc<FunctionDef>, EvalError> {
        if let Some(def) = self.functions.get(&id) {
            return Ok(def.clone());
        }
        let def = self
            .catalog
            .function_by_id(id)
            .ok_or_else(|| EvalError::Unsupported(format!("unknown function {}", id.get())))?;
        self.functions.insert(id, def.clone());
        Ok(def)
    }

    /// Look up a table definition.
    fn table(&self, table: &TableRef) -> Result<Arc<TableDef>, EvalError> {
        let TableRef::Id(id) = table else {
            return Err(EvalError::Unsupported("table referenced by name".into()));
        };
        self.catalog
            .table_by_id(*id)
            .ok_or_else(|| EvalError::Unsupported(format!("unknown table {}", id.get())))
    }

    /// Evaluate output items over each row.
    fn project_rows(
        &mut self,
        input: &Relation,
        exprs: &[NamedExpr],
        outer: &[Frame<'_>],
    ) -> Result<Vec<Vec<Datum>>, EvalError> {
        input
            .rows
            .iter()
            .map(|row| {
                let frames = frames(&input.layout, row, outer);
                exprs.iter().map(|e| self.eval(&e.expr, &frames)).collect()
            })
            .collect()
    }

    /// Keep the rows for which `predicate` is TRUE.
    fn filter_rows(
        &mut self,
        input: Relation,
        predicate: &Expr,
        outer: &[Frame<'_>],
    ) -> Result<Relation, EvalError> {
        let mut rows = Vec::new();
        for row in input.rows {
            let keep =
                truth(&self.eval(predicate, &frames(&input.layout, &row, outer))?)? == Some(true);
            if keep {
                rows.push(row);
            }
        }
        Ok(Relation {
            layout: input.layout,
            rows,
        })
    }

    /// Evaluate a `LIMIT` or `OFFSET` count; `None` when absent or NULL.
    fn count(
        &mut self,
        expr: Option<&Expr>,
        what: &'static str,
    ) -> Result<Option<usize>, EvalError> {
        let Some(expr) = expr else { return Ok(None) };
        match self.eval(expr, &[])? {
            Datum::Null => Ok(None),
            Datum::Int8(n) if n < 0 => Err(EvalError::NegativeLimit(what)),
            Datum::Int8(n) => Ok(Some(usize::try_from(n).unwrap_or(usize::MAX))),
            other => Err(EvalError::Unsupported(format!("{what} of {other:?}"))),
        }
    }

    /// Match the input rows of an update or delete to stored rows of `table`.
    ///
    /// The input carries the target table's columns under the binding of
    /// its `Get`; rows are identified by primary key (or by every column if
    /// the table has none). Returns `(input row, stored row)` pairs, each
    /// stored row once, for its first matching input row.
    fn target_rows(
        &self,
        input: &Relation,
        table: &TableDef,
    ) -> Result<Vec<(usize, usize)>, EvalError> {
        let binding = input
            .layout
            .iter()
            .map(|&(b, _)| b)
            .find(|b| self.binding_tables.get(b) == Some(&table.id))
            .ok_or_else(|| {
                EvalError::Unsupported("DML input does not read the target table".into())
            })?;
        let positions: Vec<usize> = table
            .columns
            .iter()
            .map(|c| {
                input
                    .layout
                    .iter()
                    .position(|&(b, col)| b == binding && col == c.id)
                    .ok_or_else(|| {
                        EvalError::Unsupported("target row incomplete in DML input".into())
                    })
            })
            .collect::<Result<_, _>>()?;
        let key: Vec<usize> = if table.primary_key.is_empty() {
            (0..table.columns.len()).collect()
        } else {
            table
                .primary_key
                .iter()
                .map(|c| usize::try_from(c.get()).expect("column id"))
                .collect()
        };
        let stored = self.db.rows(table.id);
        let mut matched = Vec::new();
        let mut seen = HashSet::new();
        for (i, row) in input.rows.iter().enumerate() {
            let found = stored
                .iter()
                .position(|s| key.iter().all(|&k| s[k].not_distinct(&row[positions[k]])));
            if let Some(index) = found
                && seen.insert(index)
            {
                matched.push((i, index));
            }
        }
        Ok(matched)
    }

    /// Return the layout `rel` produces, without evaluating it.
    fn layout(&self, rel: &Rel) -> Result<Layout, EvalError> {
        Ok(match rel {
            Rel::Get { table, binding, .. } => {
                let def = self.table(table)?;
                let binding = binding
                    .ok_or_else(|| EvalError::Unsupported("get without a binding".into()))?;
                def.columns.iter().map(|c| (binding, c.id)).collect()
            }
            Rel::Filter { input, .. }
            | Rel::PolicyFilter { input, .. }
            | Rel::WriteCheck { input, .. }
            | Rel::Sort { input, .. }
            | Rel::Limit { input, .. }
            | Rel::Distinct { input } => self.layout(input)?,
            Rel::Project { exprs, binding, .. } => layout_of(*binding, exprs.len())?,
            Rel::Aggregate {
                keys,
                aggs,
                binding,
                ..
            } => layout_of(*binding, keys.len() + aggs.len())?,
            Rel::Nest { keys, binding, .. } => layout_of(*binding, keys.len() + 1)?,
            Rel::Values {
                columns, binding, ..
            } => layout_of(*binding, columns.len())?,
            Rel::SetOp { left, binding, .. } => layout_of(*binding, self.layout(left)?.len())?,
            Rel::Insert {
                returning, binding, ..
            }
            | Rel::Update {
                returning, binding, ..
            }
            | Rel::Delete {
                returning, binding, ..
            } => layout_of(*binding, returning.len())?,
            Rel::Join {
                kind, left, right, ..
            } => {
                let mut layout = self.layout(left)?;
                if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                    layout.extend(self.layout(right)?);
                }
                layout
            }
            Rel::Apply { kind, outer, inner } => {
                let mut layout = self.layout(outer)?;
                if !matches!(kind, ApplyKind::Semi | ApplyKind::Anti) {
                    layout.extend(self.layout(inner)?);
                }
                layout
            }
            Rel::Unnest {
                input,
                columns,
                binding,
                ..
            } => {
                let mut layout = self.layout(input)?;
                layout.extend(layout_of(*binding, columns.len())?);
                layout
            }
        })
    }

    /// Check NOT NULL columns, primary key and unique index uniqueness of `table` after a change.
    fn check_constraints(&self, table: &TableDef, rows: &[Vec<Datum>]) -> Result<(), EvalError> {
        for row in rows {
            for column in &table.columns {
                if !column.ty.optional
                    && row[usize::try_from(column.id.get()).expect("column id")].is_null()
                {
                    return Err(EvalError::NotNull {
                        table: table.name.to_string(),
                        column: column.name.to_string(),
                    });
                }
            }
        }
        // Unique indexes: NULLs are distinct, so only rows with every indexed value set can collide.
        for index in table.indexes.iter().filter(|i| i.unique) {
            let mut keys = HashSet::new();
            for row in rows {
                let key: Vec<Datum> = index
                    .columns
                    .iter()
                    .map(|c| row[usize::try_from(c.column.get()).expect("column id")].clone())
                    .collect();
                if !key.iter().any(Datum::is_null) && !keys.insert(RowKey(key)) {
                    return Err(EvalError::UniqueViolation(table.name.to_string()));
                }
            }
        }
        if !table.primary_key.is_empty() {
            let mut keys = HashSet::new();
            for row in rows {
                let key: Vec<Datum> = table
                    .primary_key
                    .iter()
                    .map(|c| row[usize::try_from(c.get()).expect("column id")].clone())
                    .collect();
                if !keys.insert(RowKey(key)) {
                    return Err(EvalError::UniqueViolation(table.name.to_string()));
                }
            }
        }
        Ok(())
    }

    /// Replace the rows of `table`, carry out the cascades that sets off, and
    /// restore the database if a foreign key then fails.
    fn replace_rows(&mut self, table: TableId, rows: Vec<Vec<Datum>>) -> Result<(), EvalError> {
        let snapshot = self.db.clone();
        let before = self.db.rows(table).to_vec();
        self.db.set_rows(table, rows);
        super::references::cascade(self.catalog, self.db, table, &before);
        let mut checked = super::references::check(self.catalog, self.db, table);
        // Cascades change other tables, whose foreign keys must hold too.
        for fk in self.catalog.foreign_keys() {
            if checked.is_ok() && fk.table != table && fk.referenced != table {
                checked = super::references::check(self.catalog, self.db, fk.table);
            }
        }
        if checked.is_err() {
            *self.db = snapshot;
        }
        checked
    }

    /// Evaluate `RETURNING` over changed rows of `table`, as rows of the target binding.
    fn returning(
        &mut self,
        target: &DmlTarget,
        table: &TableDef,
        rows: &[Vec<Datum>],
        returning: &[NamedExpr],
        binding: Option<BindingId>,
    ) -> Result<Relation, EvalError> {
        let target_layout = layout_of(target.binding, table.columns.len())?;
        let changed = Relation {
            layout: target_layout,
            rows: rows.to_vec(),
        };
        let out = self.project_rows(&changed, returning, &[])?;
        Ok(Relation {
            layout: layout_of(binding, returning.len())?,
            rows: out,
        })
    }

    /// Evaluate a relation; `outer` holds the rows of enclosing `Apply` outer sides.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn rel(&mut self, rel: &Rel, outer: &[Frame<'_>]) -> Result<Relation, EvalError> {
        Ok(match rel {
            Rel::Get { table, binding, .. } => {
                let def = self.table(table)?;
                let binding = binding
                    .ok_or_else(|| EvalError::Unsupported("get without a binding".into()))?;
                self.binding_tables.insert(binding, def.id);
                Relation {
                    layout: def.columns.iter().map(|c| (binding, c.id)).collect(),
                    rows: self.db.rows(def.id).to_vec(),
                }
            }
            Rel::Filter { input, predicate }
            | Rel::PolicyFilter {
                input,
                residual: predicate,
            } => {
                let input = self.rel(input, outer)?;
                self.filter_rows(input, predicate, outer)?
            }
            Rel::WriteCheck { input, check } => {
                let input = self.rel(input, outer)?;
                for row in &input.rows {
                    if truth(&self.eval(check, &frames(&input.layout, row, outer))?)? != Some(true)
                    {
                        return Err(EvalError::PermissionDenied);
                    }
                }
                input
            }
            Rel::Project {
                input,
                exprs,
                binding,
            } => {
                let input = self.rel(input, outer)?;
                let rows = self.project_rows(&input, exprs, outer)?;
                Relation {
                    layout: layout_of(*binding, exprs.len())?,
                    rows,
                }
            }
            Rel::Join {
                kind,
                left,
                right,
                on,
            } => {
                let left = self.rel(left, outer)?;
                let right = self.rel(right, outer)?;
                let layout: Layout = left.layout.iter().chain(&right.layout).copied().collect();
                let left_nulls = vec![Datum::Null; left.layout.len()];
                let right_nulls = vec![Datum::Null; right.layout.len()];
                let mut right_matched = vec![false; right.rows.len()];
                let mut rows = Vec::new();
                for l in &left.rows {
                    let mut matched = false;
                    for (j, r) in right.rows.iter().enumerate() {
                        let combined: Vec<Datum> = l.iter().chain(r).cloned().collect();
                        if truth(&self.eval(on, &frames(&layout, &combined, outer))?)? == Some(true)
                        {
                            matched = true;
                            right_matched[j] = true;
                            if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                                break;
                            }
                            rows.push(combined);
                        }
                    }
                    match kind {
                        JoinKind::Semi if matched => rows.push(l.clone()),
                        JoinKind::Anti if !matched => rows.push(l.clone()),
                        JoinKind::Left | JoinKind::Full if !matched => {
                            rows.push(l.iter().chain(&right_nulls).cloned().collect());
                        }
                        _ => {}
                    }
                }
                if matches!(kind, JoinKind::Right | JoinKind::Full) {
                    for (r, matched) in right.rows.iter().zip(right_matched) {
                        if !matched {
                            rows.push(left_nulls.iter().chain(r).cloned().collect());
                        }
                    }
                }
                let layout = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                    left.layout
                } else {
                    layout
                };
                Relation { layout, rows }
            }
            Rel::Apply {
                kind,
                outer: outer_rel,
                inner,
            } => {
                let left = self.rel(outer_rel, outer)?;
                let width = self.layout(inner)?.len();
                let mut rows = Vec::new();
                for l in &left.rows {
                    let frames = frames(&left.layout, l, outer);
                    let result = self.rel(inner, &frames)?;
                    if matches!(kind, ApplyKind::Single) && result.rows.len() > 1 {
                        return Err(EvalError::CardinalityViolation);
                    }
                    match kind {
                        ApplyKind::Semi if !result.rows.is_empty() => rows.push(l.clone()),
                        ApplyKind::Anti if result.rows.is_empty() => rows.push(l.clone()),
                        ApplyKind::Semi | ApplyKind::Anti => {}
                        ApplyKind::Left | ApplyKind::Single if result.rows.is_empty() => {
                            rows.push(
                                l.iter()
                                    .cloned()
                                    .chain(std::iter::repeat_n(Datum::Null, width))
                                    .collect(),
                            );
                        }
                        _ => rows.extend(
                            result
                                .rows
                                .iter()
                                .map(|r| l.iter().chain(r).cloned().collect()),
                        ),
                    }
                }
                let layout = self.layout(rel)?;
                Relation { layout, rows }
            }
            Rel::Aggregate {
                input,
                keys,
                aggs,
                binding,
            } => {
                let input = self.rel(input, outer)?;
                // Groups in order of first appearance; without keys, exactly one group.
                let mut order: Vec<RowKey> = Vec::new();
                let mut groups: HashMap<RowKey, Vec<usize>> = HashMap::new();
                for (i, row) in input.rows.iter().enumerate() {
                    let frames = frames(&input.layout, row, outer);
                    let key = RowKey(
                        keys.iter()
                            .map(|k| self.eval(&k.expr, &frames))
                            .collect::<Result<_, _>>()?,
                    );
                    groups
                        .entry(key.clone())
                        .or_insert_with(|| {
                            order.push(key);
                            Vec::new()
                        })
                        .push(i);
                }
                if keys.is_empty() && order.is_empty() {
                    order.push(RowKey(Vec::new()));
                    groups.insert(RowKey(Vec::new()), Vec::new());
                }
                let mut rows = Vec::new();
                for key in order {
                    let members = &groups[&key];
                    let mut out = key.0.clone();
                    for item in aggs {
                        let FunctionRef::Id(id) = &item.agg.func else {
                            return Err(EvalError::Unsupported("aggregate by name".into()));
                        };
                        let def = self.function(*id)?;
                        let mut args = Vec::new();
                        for &i in members {
                            let frames = frames(&input.layout, &input.rows[i], outer);
                            if let Some(filter) = &item.agg.filter
                                && truth(&self.eval(filter, &frames)?)? != Some(true)
                            {
                                continue;
                            }
                            args.push(
                                item.agg
                                    .args
                                    .iter()
                                    .map(|a| self.eval(a, &frames))
                                    .collect::<Result<Vec<_>, _>>()?,
                            );
                        }
                        out.push(Self::aggregate(&def, args, item.agg.distinct)?);
                    }
                    rows.push(out);
                }
                Relation {
                    layout: layout_of(*binding, keys.len() + aggs.len())?,
                    rows,
                }
            }
            Rel::Distinct { input } => {
                let input = self.rel(input, outer)?;
                Relation {
                    layout: input.layout,
                    rows: dedup(input.rows),
                }
            }
            Rel::Sort { input, keys } => {
                let input = self.rel(input, outer)?;
                let mut keyed = Vec::new();
                for row in input.rows {
                    let frames = frames(&input.layout, &row, outer);
                    let values: Vec<Datum> = keys
                        .iter()
                        .map(|k| self.eval(&k.expr, &frames))
                        .collect::<Result<_, _>>()?;
                    keyed.push((values, row));
                }
                keyed.sort_by(|(a, _), (b, _)| compare_sort_keys(a, b, keys));
                Relation {
                    layout: input.layout,
                    rows: keyed.into_iter().map(|(_, r)| r).collect(),
                }
            }
            Rel::Limit {
                input,
                limit,
                offset,
            } => {
                let input = self.rel(input, outer)?;
                let limit = self.count(limit.as_ref(), "LIMIT")?;
                let offset = self.count(offset.as_ref(), "OFFSET")?.unwrap_or(0);
                let rows = input
                    .rows
                    .into_iter()
                    .skip(offset)
                    .take(limit.unwrap_or(usize::MAX))
                    .collect();
                Relation {
                    layout: input.layout,
                    rows,
                }
            }
            Rel::SetOp {
                op,
                all,
                left,
                right,
                binding,
            } => {
                let left = self.rel(left, outer)?;
                let right = self.rel(right, outer)?;
                let rows = match (op, all) {
                    (SetOpKind::Union, true) => left.rows.into_iter().chain(right.rows).collect(),
                    (SetOpKind::Union, false) => {
                        dedup(left.rows.into_iter().chain(right.rows).collect())
                    }
                    (SetOpKind::Intersect, all) => {
                        let mut available = counts(&right.rows);
                        let rows: Vec<Vec<Datum>> = left
                            .rows
                            .into_iter()
                            .filter(|r| match available.get_mut(&RowKey(r.clone())) {
                                Some(n) if *n > 0 => {
                                    *n -= 1;
                                    true
                                }
                                _ => false,
                            })
                            .collect();
                        if *all { rows } else { dedup(rows) }
                    }
                    (SetOpKind::Except, true) => {
                        let mut remove = counts(&right.rows);
                        left.rows
                            .into_iter()
                            .filter(|r| match remove.get_mut(&RowKey(r.clone())) {
                                Some(n) if *n > 0 => {
                                    *n -= 1;
                                    false
                                }
                                _ => true,
                            })
                            .collect()
                    }
                    (SetOpKind::Except, false) => {
                        let remove = counts(&right.rows);
                        dedup(
                            left.rows
                                .into_iter()
                                .filter(|r| !remove.contains_key(&RowKey(r.clone())))
                                .collect(),
                        )
                    }
                };
                Relation {
                    layout: layout_of(*binding, left.layout.len())?,
                    rows,
                }
            }
            Rel::Values {
                columns,
                rows,
                binding,
            } => {
                let rows = rows
                    .iter()
                    .map(|row| row.iter().map(|e| self.eval(e, outer)).collect())
                    .collect::<Result<_, _>>()?;
                Relation {
                    layout: layout_of(*binding, columns.len())?,
                    rows,
                }
            }
            Rel::Nest {
                input,
                keys,
                fields,
                binding,
                ..
            } => {
                let input = self.rel(input, outer)?;
                let mut order: Vec<RowKey> = Vec::new();
                let mut groups: HashMap<RowKey, Vec<Datum>> = HashMap::new();
                for row in &input.rows {
                    let frames = frames(&input.layout, row, outer);
                    let key = RowKey(
                        keys.iter()
                            .map(|k| self.eval(&k.expr, &frames))
                            .collect::<Result<_, _>>()?,
                    );
                    let record = Datum::Record(
                        fields
                            .iter()
                            .map(|f| self.eval(&f.expr, &frames))
                            .collect::<Result<_, _>>()?,
                    );
                    groups
                        .entry(key.clone())
                        .or_insert_with(|| {
                            order.push(key);
                            Vec::new()
                        })
                        .push(record);
                }
                if keys.is_empty() && order.is_empty() {
                    order.push(RowKey(Vec::new()));
                    groups.insert(RowKey(Vec::new()), Vec::new());
                }
                let rows = order
                    .into_iter()
                    .map(|key| {
                        let records = groups.remove(&key).expect("group exists");
                        let mut row = key.0;
                        row.push(Datum::Array(records));
                        row
                    })
                    .collect();
                Relation {
                    layout: layout_of(*binding, keys.len() + 1)?,
                    rows,
                }
            }
            Rel::Unnest {
                input,
                expr,
                columns,
                binding,
            } => {
                let input = self.rel(input, outer)?;
                let mut layout = input.layout.clone();
                layout.extend(layout_of(*binding, columns.len())?);
                let mut rows = Vec::new();
                for row in &input.rows {
                    let value = self.eval(expr, &frames(&input.layout, row, outer))?;
                    let Datum::Array(elements) = value else {
                        continue;
                    };
                    for element in elements {
                        let values = match element {
                            Datum::Record(fields) => fields,
                            Datum::Null if columns.len() > 1 => vec![Datum::Null; columns.len()],
                            scalar => vec![scalar],
                        };
                        rows.push(row.iter().cloned().chain(values).collect());
                    }
                }
                Relation { layout, rows }
            }
            Rel::Insert {
                target,
                columns,
                input,
                returning,
                binding,
            } => {
                let input = self.rel(input, outer)?;
                let table = self.table(&target.table)?;
                let mut new_rows = Vec::new();
                for row in &input.rows {
                    let mut full = vec![Datum::Null; table.columns.len()];
                    for (column, value) in columns.iter().zip(row) {
                        let TargetColumn::Id(id) = column else {
                            return Err(EvalError::Unsupported("insert column by name".into()));
                        };
                        full[usize::try_from(id.get()).expect("column id")] = value.clone();
                    }
                    new_rows.push(full);
                }
                let mut all: Vec<Vec<Datum>> = self.db.rows(table.id).to_vec();
                all.extend(new_rows.iter().cloned());
                self.check_constraints(&table, &all)?;
                self.replace_rows(table.id, all)?;
                self.returning(target, &table, &new_rows, returning, *binding)?
            }
            Rel::Update {
                target,
                input,
                assignments,
                returning,
                binding,
            } => {
                let input = self.rel(input, outer)?;
                let table = self.table(&target.table)?;
                let matched = self.target_rows(&input, &table)?;
                let mut all: Vec<Vec<Datum>> = self.db.rows(table.id).to_vec();
                let mut changed = Vec::new();
                for (row_index, index) in matched {
                    let frames = frames(&input.layout, &input.rows[row_index], outer);
                    let mut new = all[index].clone();
                    for assignment in assignments {
                        let TargetColumn::Id(id) = &assignment.column else {
                            return Err(EvalError::Unsupported("update column by name".into()));
                        };
                        new[usize::try_from(id.get()).expect("column id")] =
                            self.eval(&assignment.value, &frames)?;
                    }
                    all[index] = new.clone();
                    changed.push(new);
                }
                self.check_constraints(&table, &all)?;
                self.replace_rows(table.id, all)?;
                self.returning(target, &table, &changed, returning, *binding)?
            }
            Rel::Delete {
                target,
                input,
                returning,
                binding,
            } => {
                let input = self.rel(input, outer)?;
                let table = self.table(&target.table)?;
                let matched: HashSet<usize> = self
                    .target_rows(&input, &table)?
                    .into_iter()
                    .map(|(_, s)| s)
                    .collect();
                let all: Vec<Vec<Datum>> = self.db.rows(table.id).to_vec();
                let (deleted, kept): (Vec<_>, Vec<_>) = all
                    .into_iter()
                    .enumerate()
                    .partition(|(i, _)| matched.contains(i));
                self.replace_rows(table.id, kept.into_iter().map(|(_, r)| r).collect())?;
                let deleted: Vec<Vec<Datum>> = deleted.into_iter().map(|(_, r)| r).collect();
                self.returning(target, &table, &deleted, returning, *binding)?
            }
        })
    }
}
