//! Physical plans and the planner that builds them from optimized IR (ADR-0028).

use std::collections::HashMap;
use std::sync::Arc;

use arrow_schema::{Schema, SchemaRef};
use interlaken_ir::catalog::{Catalog, TableDef};
use interlaken_ir::optimize::is_leakproof;
use interlaken_ir::verify::column_types;
use interlaken_ir::{BindingId, ColumnId, Expr, Plan, Rel, TableRef, Type};

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

/// The physical operators of this milestone.
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
}

/// Rows per storage call: matches the storage layer's default scan limit.
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

impl Planner<'_> {
    /// Return the IR types and Arrow schema for `layout`.
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
            .map(|((b, c), ty)| field(&format!("c{}_{}", b.get(), c.get()), ty))
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

    /// Build the physical form of `rel`.
    fn build(&self, rel: &Rel) -> Result<Physical, ExecError> {
        match rel {
            Rel::Get { table, binding, .. } => self.scan(table, *binding, None),
            Rel::Filter { input, predicate } => {
                if let Rel::Get { table, binding, .. } = input.as_ref() {
                    // Leakproof conjuncts run in the storage scan; the rest above it.
                    let (pushed, kept): (Vec<Expr>, Vec<Expr>) = conjuncts(predicate)
                        .into_iter()
                        .partition(|c| is_leakproof(c, self.plan, self.catalog));
                    let scan = self.scan(table, *binding, conjoin(pushed))?;
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
            Rel::Project {
                input,
                exprs,
                binding,
            } => {
                let input = self.build(input)?;
                let binding = binding
                    .ok_or_else(|| ExecError::Unsupported("project without a binding".into()))?;
                let layout = (0..exprs.len())
                    .map(|i| {
                        (
                            binding,
                            ColumnId::new(u32::try_from(i).expect("column positions fit in u32")),
                        )
                    })
                    .collect();
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
                let input = self.build(input)?;
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
