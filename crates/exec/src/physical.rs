//! Physical plans and the planner that builds them from optimized IR (ADR-0028).
//!
//! Planning is heuristic: there are no statistics yet.
//!
//! - A filter directly above a scan has its leakproof conjuncts on the
//!   scan's own columns pushed into the storage scan.
//! - A join's equality conjuncts between leakproof expressions of one side
//!   each become hash keys; the rest of the condition is a residual
//!   evaluated on key-matching pairs. Without keys the join loops over all
//!   pairs. The build side is always the right input, chosen at plan time.
//! - A limit directly above a sort makes the sort keep only the rows the
//!   limit can return (top-N).
//! - Security (ADR-0025): a chain of filters and policy filters directly
//!   above a scan is planned as one. Leakproof conjuncts on the scan's own
//!   columns, policy residual conjuncts included, run in storage, so rows
//!   they reject never leave the storage pool. Everything else stays where
//!   it was, so nothing that can raise runs before a policy filter above
//!   it. A `WriteCheck` checks each batch of DML input before it is written.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arrow_schema::{Schema, SchemaRef};
use interlaken_ir::catalog::{Catalog, FunctionDef, TableDef};
use interlaken_ir::optimize::is_leakproof;
use interlaken_ir::verify::{column_types, expr_type};
use interlaken_ir::{
    ApplyKind, BinaryOp, BindingId, ColumnId, ColumnRef, DmlTarget, Expr, FunctionRef, JoinKind,
    NamedExpr, Plan, Rel, SetOpKind, SortKey, TableRef, TargetColumn, Type,
};

use crate::access::{Access, choose};
use crate::columnar::field;
use crate::error::ExecError;

/// Which (binding, column) each column of a batch holds, in order.
pub type Layout = Vec<(BindingId, ColumnId)>;

/// One physical operator with its output layout and Arrow schema.
#[derive(Clone, Debug)]
pub struct Physical {
    /// The operator's position in a preorder walk of the plan, from 0 at the
    /// root; [`Metrics`](crate::Metrics) are indexed by it.
    pub id: usize,
    /// The operator.
    pub op: Op,
    /// What each output column is.
    pub layout: Layout,
    /// The output schema.
    pub schema: SchemaRef,
    /// The IR type of each output column.
    pub types: Vec<Type>,
}

/// One aggregate of a [`Op::Aggregate`].
#[derive(Clone, Debug)]
pub struct Aggregate {
    /// The aggregate function.
    pub def: Arc<FunctionDef>,
    /// The argument expressions; empty for `count(*)`.
    pub args: Vec<Expr>,
    /// Aggregate distinct argument tuples only.
    pub distinct: bool,
    /// Rows where this is not TRUE are skipped.
    pub filter: Option<Expr>,
}

/// The physical operators.
#[derive(Clone, Debug)]
pub enum Op {
    /// Every visible row of a table in primary key order, read in batches.
    TableScan {
        /// The table.
        table: Arc<TableDef>,
        /// How the rows are reached.
        access: Access,
        /// Leakproof conjuncts evaluated on the storage thread (ADR-0023).
        pushed: Option<Expr>,
        /// Policy residual conjuncts evaluated on the storage thread (ADR-0025).
        policy: Option<Expr>,
        /// Whether a policy filter governs the scan; `EXPLAIN` then hides
        /// its key values from principals who may not see policies.
        secured: bool,
        /// Rows per storage call.
        batch_rows: usize,
    },
    /// Rows where the predicate is TRUE.
    Filter {
        /// The input.
        input: Box<Physical>,
        /// The predicate.
        predicate: Expr,
    },
    /// Computed columns.
    Project {
        /// The input.
        input: Box<Physical>,
        /// The output expressions.
        exprs: Vec<Expr>,
    },
    /// Skip `offset` rows, then return at most `limit`; stops pulling once satisfied.
    Limit {
        /// The input.
        input: Box<Physical>,
        /// The row limit, evaluated when execution starts.
        limit: Option<Expr>,
        /// The offset, evaluated when execution starts.
        offset: Option<Expr>,
    },
    /// A hash join (with keys) or nested-loop join (without), building on the right input.
    Join {
        /// The join kind.
        kind: JoinKind,
        /// The probe input.
        left: Box<Physical>,
        /// The build input, read completely first.
        right: Box<Physical>,
        /// Equality keys: (left expression, right expression), both leakproof.
        keys: Vec<(Expr, Expr)>,
        /// The rest of the join condition, evaluated on key-matching pairs.
        residual: Option<Expr>,
    },
    /// A correlated nested loop: the inner plan runs once per outer row.
    Apply {
        /// The apply kind.
        kind: ApplyKind,
        /// The outer input.
        outer: Box<Physical>,
        /// The inner plan, which may read the outer row's columns.
        inner: Box<Physical>,
    },
    /// Hash aggregation: one row per group of `keys` (exactly one without keys).
    Aggregate {
        /// The input.
        input: Box<Physical>,
        /// The grouping expressions.
        keys: Vec<Expr>,
        /// The aggregates.
        aggs: Vec<Aggregate>,
    },
    /// The first of each set of duplicate rows.
    Distinct {
        /// The input.
        input: Box<Physical>,
    },
    /// An in-memory sort, keeping only the first rows if `fetch` is set (top-N).
    Sort {
        /// The input.
        input: Box<Physical>,
        /// The sort keys.
        keys: Vec<SortKey>,
        /// The (limit, offset) of a limit directly above, bounding how many rows to keep.
        fetch: Option<(Option<Expr>, Option<Expr>)>,
    },
    /// `UNION`, `INTERSECT` or `EXCEPT`, hash-based; `all` keeps duplicates.
    SetOp {
        /// The operation.
        op: SetOpKind,
        /// Keep duplicates.
        all: bool,
        /// The left input.
        left: Box<Physical>,
        /// The right input.
        right: Box<Physical>,
    },
    /// Literal (or correlated) rows.
    Values {
        /// The rows' expressions.
        rows: Vec<Vec<Expr>>,
    },
    /// One row per element of an array, after the input row's columns.
    Unnest {
        /// The input.
        input: Box<Physical>,
        /// The array expression.
        expr: Expr,
        /// The element's columns: a record's fields, or one for a scalar.
        width: usize,
    },
    /// One row per group of `keys`, with the group's `fields` collected into a list of records.
    Nest {
        /// The input.
        input: Box<Physical>,
        /// The grouping expressions.
        keys: Vec<Expr>,
        /// The record fields.
        fields: Vec<Expr>,
    },
    /// A security barrier: rows where the policy residual is TRUE (ADR-0025).
    PolicyFilter {
        /// The input.
        input: Box<Physical>,
        /// The residual conjuncts not run in storage.
        residual: Expr,
        /// The table whose rows the policy governs, for `EXPLAIN`.
        table: String,
    },
    /// Fail the statement unless every input row passes the check (ADR-0025).
    WriteCheck {
        /// The input.
        input: Box<Physical>,
        /// The check.
        check: Expr,
        /// The table written, named in the permission error.
        table: String,
    },
    /// Write a row per input row (ADR-0026).
    Insert {
        /// What is written and returned.
        dml: Dml,
        /// The table column each input column fills; the rest are NULL.
        columns: Vec<usize>,
    },
    /// Rewrite each target row the input identifies, once.
    Update {
        /// What is written and returned.
        dml: Dml,
        /// The input position of each table column of the target row.
        key: Vec<usize>,
        /// (table column, new value over the input) pairs.
        assignments: Vec<(usize, Expr)>,
    },
    /// Delete each target row the input identifies, once.
    Delete {
        /// What is written and returned.
        dml: Dml,
        /// The input position of each table column of the target row.
        key: Vec<usize>,
    },
}

/// What a DML operator writes and returns.
#[derive(Clone, Debug)]
pub struct Dml {
    /// The target table.
    pub table: Arc<TableDef>,
    /// The input.
    pub input: Box<Physical>,
    /// The `RETURNING` expressions, over the target row.
    pub returning: Vec<Expr>,
    /// The target row's columns as `RETURNING` sees them.
    pub target: Layout,
}

impl Op {
    /// Return the operator's inputs, left to right.
    #[must_use]
    pub fn inputs(&self) -> Vec<&Physical> {
        match self {
            Self::TableScan { .. } | Self::Values { .. } => Vec::new(),
            Self::Filter { input, .. }
            | Self::Project { input, .. }
            | Self::Limit { input, .. }
            | Self::Aggregate { input, .. }
            | Self::Distinct { input }
            | Self::Sort { input, .. }
            | Self::Unnest { input, .. }
            | Self::Nest { input, .. }
            | Self::PolicyFilter { input, .. }
            | Self::WriteCheck { input, .. } => vec![input],
            Self::Join { left, right, .. } | Self::SetOp { left, right, .. } => vec![left, right],
            Self::Apply { outer, inner, .. } => vec![outer, inner],
            Self::Insert { dml, .. } | Self::Update { dml, .. } | Self::Delete { dml, .. } => {
                vec![&dml.input]
            }
        }
    }

    /// Return the operator's inputs, left to right, for modification.
    fn inputs_mut(&mut self) -> Vec<&mut Physical> {
        match self {
            Self::TableScan { .. } | Self::Values { .. } => Vec::new(),
            Self::Filter { input, .. }
            | Self::Project { input, .. }
            | Self::Limit { input, .. }
            | Self::Aggregate { input, .. }
            | Self::Distinct { input }
            | Self::Sort { input, .. }
            | Self::Unnest { input, .. }
            | Self::Nest { input, .. }
            | Self::PolicyFilter { input, .. }
            | Self::WriteCheck { input, .. } => vec![input],
            Self::Join { left, right, .. } | Self::SetOp { left, right, .. } => vec![left, right],
            Self::Apply { outer, inner, .. } => vec![outer, inner],
            Self::Insert { dml, .. } | Self::Update { dml, .. } | Self::Delete { dml, .. } => {
                vec![&mut dml.input]
            }
        }
    }
}

impl Physical {
    /// Return the number of operators in the plan rooted here.
    #[must_use]
    pub fn size(&self) -> usize {
        1 + self.op.inputs().iter().map(|i| i.size()).sum::<usize>()
    }
}

/// Number the operators of `node` in preorder, starting at `next`.
fn number(node: &mut Physical, next: &mut usize) {
    node.id = *next;
    *next += 1;
    for input in node.op.inputs_mut() {
        number(input, next);
    }
}

/// Rows per storage call and per output batch.
pub const BATCH_ROWS: usize = 1024;

/// Settings for physical planning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanOptions {
    /// Rows per storage call in scans.
    pub batch_rows: usize,
}

impl Default for PlanOptions {
    /// Use the storage layer's default batch size.
    fn default() -> Self {
        Self {
            batch_rows: BATCH_ROWS,
        }
    }
}

/// Builds a physical plan from one optimized IR plan.
struct Planner<'a> {
    /// The catalog.
    catalog: &'a dyn Catalog,
    /// The IR plan, for leakproofness checks.
    plan: &'a Plan,
    /// The type of every column the plan introduces.
    types: HashMap<(BindingId, ColumnId), Type>,
    /// The same, as a list, for typing expressions over any columns.
    columns: Vec<(BindingId, ColumnId, Type)>,
    /// Settings.
    options: PlanOptions,
}

/// Split `expr` into its conjuncts.
fn conjuncts(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::And(items) => items.iter().flat_map(conjuncts).collect(),
        other => vec![other.clone()],
    }
}

/// Combine conjuncts (`None` when there are none).
fn conjoin(mut items: Vec<Expr>) -> Option<Expr> {
    match items.len() {
        0 => None,
        1 => items.pop(),
        _ => Some(Expr::And(items)),
    }
}

/// Collect the bindings of every column `expr` reads.
fn bindings_read(expr: &Expr, out: &mut HashSet<BindingId>) {
    match expr {
        Expr::Column(ColumnRef::Bound { binding, .. }) => {
            out.insert(*binding);
        }
        Expr::Column(ColumnRef::Named { .. })
        | Expr::Literal(_)
        | Expr::Param { .. }
        | Expr::Subquery { .. } => {}
        Expr::Call { args: items, .. }
        | Expr::And(items)
        | Expr::Or(items)
        | Expr::Coalesce(items)
        | Expr::Array(items) => items.iter().for_each(|e| bindings_read(e, out)),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => bindings_read(expr, out),
        Expr::Binary { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::Try {
            expr: left,
            default: right,
        } => {
            bindings_read(left, out);
            bindings_read(right, out);
        }
        Expr::Case {
            operand,
            branches,
            otherwise,
        } => {
            for e in operand.iter().chain(otherwise.iter()) {
                bindings_read(e, out);
            }
            for b in branches {
                bindings_read(&b.when, out);
                bindings_read(&b.then, out);
            }
        }
    }
}

/// Return whether every column `expr` reads has a binding in `allowed`.
fn reads_only(expr: &Expr, allowed: &HashSet<BindingId>) -> bool {
    let mut read = HashSet::new();
    bindings_read(expr, &mut read);
    read.is_subset(allowed)
}

/// Return whether `expr` reads any column with a binding in `bindings`.
fn reads_any(expr: &Expr, bindings: &HashSet<BindingId>) -> bool {
    let mut read = HashSet::new();
    bindings_read(expr, &mut read);
    !read.is_disjoint(bindings)
}

/// Collect the bindings of every scan of `table` in `rel`.
fn scans_of(rel: &Rel, table: &TableRef, out: &mut Vec<BindingId>) {
    if let Rel::Get {
        table: t,
        binding: Some(b),
        ..
    } = rel
        && t == table
    {
        out.push(*b);
    }
    for input in rel.inputs() {
        scans_of(input, table, out);
    }
}

/// Return where the target row's columns are in an update's or delete's input.
///
/// The input reads the target table; as in the interpreter, the target row
/// is the first scan of it whose columns are all in the input.
fn target_row(dml: &Dml, input: &Rel) -> Result<Vec<usize>, ExecError> {
    let mut scans = Vec::new();
    scans_of(input, &TableRef::Id(dml.table.id), &mut scans);
    let layout = &dml.input.layout;
    let mut bindings: Vec<BindingId> = Vec::new();
    for (b, _) in layout {
        if scans.contains(b) && !bindings.contains(b) {
            bindings.push(*b);
        }
    }
    bindings
        .into_iter()
        .find_map(|b| {
            dml.table
                .columns
                .iter()
                .map(|c| layout.iter().position(|&(lb, lc)| lb == b && lc == c.id))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| ExecError::Unsupported("DML input does not carry the target row".into()))
}

/// Return the name of the table whose rows a policy filter over `rel` governs:
/// the first table scanned along the leftmost (outer) inputs.
fn governed_table(rel: &Rel, catalog: &dyn Catalog) -> String {
    match rel {
        Rel::Get {
            table: TableRef::Id(id),
            ..
        } => catalog.table_by_id(*id).map_or_else(
            || format!("table {}", id.get()),
            |t| t.name.last().to_string(),
        ),
        other => other
            .inputs()
            .first()
            .map_or_else(|| "?".to_owned(), |i| governed_table(i, catalog)),
    }
}

/// Return the distinct bindings of a layout.
fn bindings_of(layout: &Layout) -> HashSet<BindingId> {
    layout.iter().map(|(b, _)| *b).collect()
}

/// Return the layout of `n` columns of `binding`.
fn columns_of(binding: Option<BindingId>, n: usize) -> Result<Layout, ExecError> {
    let binding =
        binding.ok_or_else(|| ExecError::Unsupported("operator without a binding".into()))?;
    Ok((0..n)
        .map(|i| {
            (
                binding,
                ColumnId::new(u32::try_from(i).expect("column positions fit in u32")),
            )
        })
        .collect())
}

impl Planner<'_> {
    /// Return the IR types and Arrow schema for `layout`.
    ///
    /// Every Arrow field is nullable: outer joins and applies pad with NULLs
    /// columns whose IR type is NOT NULL.
    fn schema(&self, layout: &Layout) -> Result<(Vec<Type>, SchemaRef), ExecError> {
        let types = layout
            .iter()
            .map(|(b, c)| {
                self.types.get(&(*b, *c)).cloned().ok_or_else(|| {
                    ExecError::Catalog(format!("no type for column #{}.{}", b.get(), c.get()))
                })
            })
            .collect::<Result<Vec<_>, ExecError>>()?;
        let fields: Vec<_> = layout
            .iter()
            .zip(&types)
            .map(|((b, c), ty)| {
                field(
                    &format!("c{}_{}", b.get(), c.get()),
                    &Type {
                        nullable: true,
                        ..ty.clone()
                    },
                )
            })
            .collect();
        Ok((types, Arc::new(Schema::new(fields))))
    }

    /// Wrap an operator with its layout, types and schema.
    fn node(&self, op: Op, layout: Layout) -> Result<Physical, ExecError> {
        let (types, schema) = self.schema(&layout)?;
        Ok(Physical {
            id: 0,
            op,
            layout,
            schema,
            types,
        })
    }

    /// Return the type of `expr` over the columns of `layout`.
    fn expr_type(&self, expr: &Expr, layout: &Layout) -> Option<Type> {
        let columns: Vec<_> = layout
            .iter()
            .filter_map(|(b, c)| Some((*b, *c, self.types.get(&(*b, *c))?.clone())))
            .collect();
        expr_type(expr, self.catalog, &columns).ok()
    }

    /// Build a table scan, with `pushed` and `policy` conjuncts evaluated in storage.
    fn scan(
        &self,
        table: &TableRef,
        binding: Option<BindingId>,
        access: Access,
        pushed: Option<Expr>,
        policy: Option<Expr>,
        secured: bool,
    ) -> Result<Physical, ExecError> {
        let def = self.table(table)?;
        let binding =
            binding.ok_or_else(|| ExecError::Unsupported("get without a binding".into()))?;
        let layout = def.columns.iter().map(|c| (binding, c.id)).collect();
        self.node(
            Op::TableScan {
                table: def,
                access,
                pushed,
                policy,
                secured,
                batch_rows: self.options.batch_rows.max(1),
            },
            layout,
        )
    }

    /// Look up a resolved table.
    fn table(&self, table: &TableRef) -> Result<Arc<TableDef>, ExecError> {
        let TableRef::Id(id) = table else {
            return Err(ExecError::Unsupported("unresolved table".into()));
        };
        self.catalog
            .table_by_id(*id)
            .ok_or_else(|| ExecError::Catalog(format!("unknown table {}", id.get())))
    }

    /// Build a chain of filters and policy filters, pushing what it can into a scan directly below.
    ///
    /// The best key bounds the scan, and leakproof conjuncts on the scan's
    /// own columns (from any filter of the chain, since they can't raise)
    /// run in storage, except under an index scan, which can't filter
    /// there. The remaining conjuncts stay in their own filter, in their
    /// original order, so a policy filter still runs before anything above
    /// it. Conjuncts used as keys are kept too: the access path only has to
    /// return a superset of the matching rows.
    fn filters(&self, rel: &Rel) -> Result<Physical, ExecError> {
        // (is a policy filter, conjuncts), top first.
        let mut stages: Vec<(bool, Vec<Expr>)> = Vec::new();
        let mut base = rel;
        loop {
            match base {
                Rel::Filter { input, predicate } => {
                    stages.push((false, conjuncts(predicate)));
                    base = input;
                }
                Rel::PolicyFilter { input, residual } => {
                    stages.push((true, conjuncts(residual)));
                    base = input;
                }
                _ => break,
            }
        }
        let mut node = match base {
            Rel::Get {
                table,
                binding: Some(binding),
                ..
            } => {
                let own = HashSet::from([*binding]);
                let def = self.table(table)?;
                let all: Vec<Expr> = stages.iter().flat_map(|(_, c)| c.clone()).collect();
                let access = choose(
                    &def,
                    *binding,
                    &all,
                    |e| !reads_any(e, &own) && is_leakproof(e, self.plan, self.catalog),
                    |e| {
                        expr_type(e, self.catalog, &self.columns)
                            .ok()
                            .map(|t| t.data)
                    },
                );
                let pushable = |c: &Expr| {
                    !matches!(access, Access::Index { .. })
                        && reads_only(c, &own)
                        && is_leakproof(c, self.plan, self.catalog)
                };
                let (mut pushed, mut policy) = (Vec::new(), Vec::new());
                for (is_policy, conjuncts) in &mut stages {
                    let (moved, kept): (Vec<Expr>, Vec<Expr>) =
                        std::mem::take(conjuncts).into_iter().partition(pushable);
                    if *is_policy {
                        policy.extend(moved);
                    } else {
                        pushed.extend(moved);
                    }
                    *conjuncts = kept;
                }
                let secured = stages.iter().any(|(is_policy, _)| *is_policy);
                self.scan(
                    table,
                    Some(*binding),
                    access,
                    conjoin(pushed),
                    conjoin(policy),
                    secured,
                )?
            }
            other => self.build(other)?,
        };
        let table = governed_table(base, self.catalog);
        for (is_policy, conjuncts) in stages.into_iter().rev() {
            let Some(predicate) = conjoin(conjuncts) else {
                continue;
            };
            let layout = node.layout.clone();
            let input = Box::new(node);
            let op = if is_policy {
                Op::PolicyFilter {
                    input,
                    residual: predicate,
                    table: table.clone(),
                }
            } else {
                Op::Filter { input, predicate }
            };
            node = self.node(op, layout)?;
        }
        Ok(node)
    }

    /// Build a join, extracting hash keys from its condition.
    fn join(
        &self,
        kind: JoinKind,
        left: &Rel,
        right: &Rel,
        on: &Expr,
    ) -> Result<Physical, ExecError> {
        let (left, right) = (self.build(left)?, self.build(right)?);
        let (left_bindings, right_bindings) =
            (bindings_of(&left.layout), bindings_of(&right.layout));
        let mut keys = Vec::new();
        let mut residual = Vec::new();
        for conjunct in conjuncts(on) {
            if let Expr::Binary {
                op: BinaryOp::Eq,
                left: a,
                right: b,
            } = &conjunct
            {
                // Keys must not raise: the build side is evaluated for every row up front.
                let leakproof = is_leakproof(a, self.plan, self.catalog)
                    && is_leakproof(b, self.plan, self.catalog);
                let side = |e: &Expr, bindings: &HashSet<BindingId>| {
                    !bindings.is_empty() && reads_only(e, bindings)
                };
                let pair = if side(a, &left_bindings) && side(b, &right_bindings) {
                    Some(((**a).clone(), (**b).clone()))
                } else if side(b, &left_bindings) && side(a, &right_bindings) {
                    Some(((**b).clone(), (**a).clone()))
                } else {
                    None
                };
                if let Some((l, r)) = pair
                    && leakproof
                    && self.expr_type(&l, &left.layout).map(|t| t.data)
                        == self.expr_type(&r, &right.layout).map(|t| t.data)
                {
                    keys.push((l, r));
                    continue;
                }
            }
            if !matches!(conjunct, Expr::Literal(interlaken_ir::Value::Bool(true))) {
                residual.push(conjunct);
            }
        }
        let mut layout = left.layout.clone();
        if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
            layout.extend(right.layout.iter().copied());
        }
        let op = Op::Join {
            kind,
            left: Box::new(left),
            right: Box::new(right),
            keys,
            residual: conjoin(residual),
        };
        self.node(op, layout)
    }

    /// Build the parts every DML operator shares.
    fn dml(
        &self,
        target: &DmlTarget,
        input: &Rel,
        returning: &[NamedExpr],
        table: Arc<TableDef>,
    ) -> Result<Dml, ExecError> {
        if table.system {
            return Err(ExecError::Unsupported(format!(
                "writing system table {}",
                table.name
            )));
        }
        let target_layout = match target.binding {
            Some(binding) => table.columns.iter().map(|c| (binding, c.id)).collect(),
            None if returning.is_empty() => Vec::new(),
            None => {
                return Err(ExecError::Unsupported(
                    "RETURNING without a target binding".into(),
                ));
            }
        };
        let mut input = self.build(input)?;
        if let Op::WriteCheck { table: checked, .. } = &mut input.op {
            *checked = table.name.last().to_string();
        }
        Ok(Dml {
            input: Box::new(input),
            returning: returning.iter().map(|e| e.expr.clone()).collect(),
            target: target_layout,
            table,
        })
    }

    /// Build the physical form of `rel`.
    fn build(&self, rel: &Rel) -> Result<Physical, ExecError> {
        match rel {
            Rel::Get { table, binding, .. } => {
                self.scan(table, *binding, Access::Full, None, None, false)
            }
            Rel::Filter { .. } | Rel::PolicyFilter { .. } => self.filters(rel),
            Rel::WriteCheck { input, check } => {
                let input = self.build(input)?;
                let layout = input.layout.clone();
                // The DML operator above names its target table (see `dml`).
                self.node(
                    Op::WriteCheck {
                        input: Box::new(input),
                        check: check.clone(),
                        table: String::new(),
                    },
                    layout,
                )
            }
            Rel::Unnest {
                input,
                expr,
                columns,
                binding,
            } => {
                let input = self.build(input)?;
                let mut layout = input.layout.clone();
                layout.extend(columns_of(*binding, columns.len())?);
                self.node(
                    Op::Unnest {
                        input: Box::new(input),
                        expr: expr.clone(),
                        width: columns.len(),
                    },
                    layout,
                )
            }
            Rel::Nest {
                input,
                keys,
                fields,
                binding,
                ..
            } => {
                let input = self.build(input)?;
                let layout = columns_of(*binding, keys.len() + 1)?;
                self.node(
                    Op::Nest {
                        input: Box::new(input),
                        keys: keys.iter().map(|k| k.expr.clone()).collect(),
                        fields: fields.iter().map(|f| f.expr.clone()).collect(),
                    },
                    layout,
                )
            }
            Rel::Project {
                input,
                exprs,
                binding,
            } => {
                let input = self.build(input)?;
                let layout = columns_of(*binding, exprs.len())?;
                let exprs = exprs.iter().map(|e| e.expr.clone()).collect();
                self.node(
                    Op::Project {
                        input: Box::new(input),
                        exprs,
                    },
                    layout,
                )
            }
            Rel::Limit {
                input,
                limit,
                offset,
            } => {
                let input = match input.as_ref() {
                    // Top-N: the sort keeps only the rows the limit can return.
                    Rel::Sort {
                        input: sorted,
                        keys,
                    } => {
                        let sorted = self.build(sorted)?;
                        let layout = sorted.layout.clone();
                        let fetch = Some((limit.clone(), offset.clone()));
                        self.node(
                            Op::Sort {
                                input: Box::new(sorted),
                                keys: keys.clone(),
                                fetch,
                            },
                            layout,
                        )?
                    }
                    other => self.build(other)?,
                };
                let layout = input.layout.clone();
                self.node(
                    Op::Limit {
                        input: Box::new(input),
                        limit: limit.clone(),
                        offset: offset.clone(),
                    },
                    layout,
                )
            }
            Rel::Join {
                kind,
                left,
                right,
                on,
            } => self.join(*kind, left, right, on),
            Rel::Apply { kind, outer, inner } => {
                let (outer, inner) = (self.build(outer)?, self.build(inner)?);
                let mut layout = outer.layout.clone();
                if !matches!(kind, ApplyKind::Semi | ApplyKind::Anti) {
                    layout.extend(inner.layout.iter().copied());
                }
                self.node(
                    Op::Apply {
                        kind: *kind,
                        outer: Box::new(outer),
                        inner: Box::new(inner),
                    },
                    layout,
                )
            }
            Rel::Aggregate {
                input,
                keys,
                aggs,
                binding,
            } => {
                let input = self.build(input)?;
                let layout = columns_of(*binding, keys.len() + aggs.len())?;
                let aggs = aggs
                    .iter()
                    .map(|a| {
                        let FunctionRef::Id(id) = &a.agg.func else {
                            return Err(ExecError::Unsupported("aggregate by name".into()));
                        };
                        let def = self.catalog.function_by_id(*id).ok_or_else(|| {
                            ExecError::Catalog(format!("unknown aggregate {}", id.get()))
                        })?;
                        Ok(Aggregate {
                            def,
                            args: a.agg.args.clone(),
                            distinct: a.agg.distinct,
                            filter: a.agg.filter.clone(),
                        })
                    })
                    .collect::<Result<_, ExecError>>()?;
                let keys = keys.iter().map(|k| k.expr.clone()).collect();
                self.node(
                    Op::Aggregate {
                        input: Box::new(input),
                        keys,
                        aggs,
                    },
                    layout,
                )
            }
            Rel::Distinct { input } => {
                let input = self.build(input)?;
                let layout = input.layout.clone();
                self.node(
                    Op::Distinct {
                        input: Box::new(input),
                    },
                    layout,
                )
            }
            Rel::Sort { input, keys } => {
                let input = self.build(input)?;
                let layout = input.layout.clone();
                self.node(
                    Op::Sort {
                        input: Box::new(input),
                        keys: keys.clone(),
                        fetch: None,
                    },
                    layout,
                )
            }
            Rel::SetOp {
                op,
                all,
                left,
                right,
                binding,
            } => {
                let (left, right) = (self.build(left)?, self.build(right)?);
                let layout = columns_of(*binding, left.layout.len())?;
                self.node(
                    Op::SetOp {
                        op: *op,
                        all: *all,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    layout,
                )
            }
            Rel::Insert {
                target,
                columns,
                input,
                returning,
                binding,
            } => {
                let table = self.table(&target.table)?;
                let positions = columns
                    .iter()
                    .map(|c| match c {
                        TargetColumn::Id(id) => usize::try_from(id.get())
                            .map_err(|_| ExecError::Catalog("column id".into())),
                        TargetColumn::Named(_) => {
                            Err(ExecError::Unsupported("insert column by name".into()))
                        }
                    })
                    .collect::<Result<_, _>>()?;
                let dml = self.dml(target, input, returning, table)?;
                self.node(
                    Op::Insert {
                        dml,
                        columns: positions,
                    },
                    columns_of(*binding, returning.len())?,
                )
            }
            Rel::Update {
                target,
                input,
                assignments,
                returning,
                binding,
            } => {
                let table = self.table(&target.table)?;
                let assignments = assignments
                    .iter()
                    .map(|a| match &a.column {
                        TargetColumn::Id(id) => Ok((
                            usize::try_from(id.get())
                                .map_err(|_| ExecError::Catalog("column id".into()))?,
                            a.value.clone(),
                        )),
                        TargetColumn::Named(_) => {
                            Err(ExecError::Unsupported("update column by name".into()))
                        }
                    })
                    .collect::<Result<_, ExecError>>()?;
                let dml = self.dml(target, input, returning, table)?;
                let key = target_row(&dml, input)?;
                self.node(
                    Op::Update {
                        dml,
                        key,
                        assignments,
                    },
                    columns_of(*binding, returning.len())?,
                )
            }
            Rel::Delete {
                target,
                input,
                returning,
                binding,
            } => {
                let table = self.table(&target.table)?;
                let dml = self.dml(target, input, returning, table)?;
                let key = target_row(&dml, input)?;
                self.node(
                    Op::Delete { dml, key },
                    columns_of(*binding, returning.len())?,
                )
            }
            Rel::Values {
                columns,
                rows,
                binding,
            } => {
                let layout = columns_of(*binding, columns.len())?;
                self.node(Op::Values { rows: rows.clone() }, layout)
            }
        }
    }
}

/// Build the physical plan for a resolved, verified (and normally optimized) IR plan.
///
/// # Errors
///
/// Returns [`ExecError::Unsupported`] for plans the executor can't run
/// (unresolved names, operators without bindings, DML into system tables),
/// or [`ExecError::Catalog`] if the plan refers to unknown tables.
pub fn plan(plan: &Plan, catalog: &dyn Catalog) -> Result<Physical, ExecError> {
    plan_with(plan, catalog, PlanOptions::default())
}

/// Build the physical plan like [`plan`], with explicit settings.
///
/// # Errors
///
/// As for [`plan`].
pub fn plan_with(
    plan: &Plan,
    catalog: &dyn Catalog,
    options: PlanOptions,
) -> Result<Physical, ExecError> {
    let types = column_types(plan, catalog);
    let columns = types
        .iter()
        .map(|((b, c), t)| (*b, *c, t.clone()))
        .collect();
    let planner = Planner {
        catalog,
        plan,
        types,
        columns,
        options,
    };
    let mut root = planner.build(&plan.root)?;
    number(&mut root, &mut 0);
    Ok(root)
}
