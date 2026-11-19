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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arrow_schema::{Schema, SchemaRef};
use interlaken_ir::catalog::{Catalog, FunctionDef, TableDef};
use interlaken_ir::optimize::is_leakproof;
use interlaken_ir::verify::{column_types, expr_type};
use interlaken_ir::{
    ApplyKind, BinaryOp, BindingId, ColumnId, ColumnRef, Expr, FunctionRef, JoinKind, Plan, Rel,
    SetOpKind, SortKey, TableRef, Type,
};

use crate::columnar::field;
use crate::error::ExecError;

/// Which (binding, column) each column of a batch holds, in order.
pub type Layout = Vec<(BindingId, ColumnId)>;

/// One physical operator with its output layout and Arrow schema.
#[derive(Clone, Debug)]
pub struct Physical {
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
        /// Leakproof conjuncts evaluated on the storage thread (ADR-0023).
        pushed: Option<Expr>,
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

    /// Build a table scan, with `pushed` conjuncts evaluated in storage.
    fn scan(
        &self,
        table: &TableRef,
        binding: Option<BindingId>,
        pushed: Option<Expr>,
    ) -> Result<Physical, ExecError> {
        let TableRef::Id(id) = table else {
            return Err(ExecError::Unsupported("unresolved table".into()));
        };
        let def = self
            .catalog
            .table_by_id(*id)
            .ok_or_else(|| ExecError::Catalog(format!("unknown table {}", id.get())))?;
        let binding =
            binding.ok_or_else(|| ExecError::Unsupported("get without a binding".into()))?;
        let layout = def.columns.iter().map(|c| (binding, c.id)).collect();
        self.node(
            Op::TableScan {
                table: def,
                pushed,
                batch_rows: self.options.batch_rows.max(1),
            },
            layout,
        )
    }

    /// Build a filter, pushing what it can into a scan directly below.
    fn filter(&self, input: &Rel, predicate: &Expr) -> Result<Physical, ExecError> {
        if let Rel::Get {
            table,
            binding: Some(binding),
            ..
        } = input
        {
            // Leakproof conjuncts on the scan's own columns run in storage; the rest above it.
            let own = HashSet::from([*binding]);
            let (pushed, kept): (Vec<Expr>, Vec<Expr>) = conjuncts(predicate)
                .into_iter()
                .partition(|c| reads_only(c, &own) && is_leakproof(c, self.plan, self.catalog));
            let scan = self.scan(table, Some(*binding), conjoin(pushed))?;
            return match conjoin(kept) {
                None => Ok(scan),
                Some(predicate) => {
                    let layout = scan.layout.clone();
                    self.node(
                        Op::Filter {
                            input: Box::new(scan),
                            predicate,
                        },
                        layout,
                    )
                }
            };
        }
        let input = self.build(input)?;
        let layout = input.layout.clone();
        self.node(
            Op::Filter {
                input: Box::new(input),
                predicate: predicate.clone(),
            },
            layout,
        )
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

    /// Build the physical form of `rel`.
    fn build(&self, rel: &Rel) -> Result<Physical, ExecError> {
        match rel {
            Rel::Get { table, binding, .. } => self.scan(table, *binding, None),
            Rel::Filter { input, predicate } => self.filter(input, predicate),
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
            Rel::Values {
                columns,
                rows,
                binding,
            } => {
                let layout = columns_of(*binding, columns.len())?;
                self.node(Op::Values { rows: rows.clone() }, layout)
            }
            other => Err(ExecError::Unsupported(format!(
                "operator {} (later executor milestones)",
                interlaken_ir::text::print_rel(other)
                    .split_whitespace()
                    .next()
                    .unwrap_or("?")
                    .trim_start_matches('(')
            ))),
        }
    }
}

/// Build the physical plan for a resolved, verified (and normally optimized) IR plan.
///
/// # Errors
///
/// Returns [`ExecError::Unsupported`] for operators later milestones add,
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
    let planner = Planner {
        catalog,
        plan,
        types: column_types(plan, catalog),
        options,
    };
    planner.build(&plan.root)
}
