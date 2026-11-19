//! Running plans against in-memory data.

use std::collections::BTreeMap;

use super::datum::Datum;
use super::error::EvalError;
use super::rel::Interp;
use crate::catalog::Catalog;
use crate::names::TableId;
use crate::rel::Plan;

/// Table contents for the interpreter: rows in insertion order, each with
/// one value per column in column-ID order.
#[derive(Clone, Debug, Default)]
pub struct Database {
    /// Rows by table; a missing table is empty.
    tables: BTreeMap<TableId, Vec<Vec<Datum>>>,
}

impl Database {
    /// Construct an empty database.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the rows of `table`.
    #[must_use]
    pub fn rows(&self, table: TableId) -> &[Vec<Datum>] {
        self.tables.get(&table).map_or(&[], Vec::as_slice)
    }

    /// Replace the rows of `table`. Constraints are not checked.
    pub fn set_rows(&mut self, table: TableId, rows: Vec<Vec<Datum>>) {
        self.tables.insert(table, rows);
    }
}

/// Settings for one execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecOptions {
    /// The value of `now()`, in microseconds since the Unix epoch.
    pub now: i64,
    /// The seed for `random()` and `gen_random_uuid()`.
    pub seed: u64,
}

/// Run a resolved, verified plan and return its rows.
///
/// DML changes `db` only if the whole plan succeeds.
///
/// # Errors
///
/// Returns an [`EvalError`] for errors Postgres would raise (overflow,
/// division by zero, failed casts, constraint violations and so on), or
/// [`EvalError::Unsupported`] for a plan that is not resolved.
///
/// # Examples
///
/// ```
/// use interlaken_ir::interp::{Database, Datum, ExecOptions, execute};
/// use interlaken_ir::text::{parse_catalog, parse_plan};
///
/// let catalog = parse_catalog("(catalog (builtins))")?;
/// let plan = parse_plan(
///     "(plan (project (values (columns x) (row 1) (row 2) :bind 1) (as (* #1.0 10) y) :bind 2) :schema-version 1)",
/// )?;
/// let rows = execute(&plan, &catalog, &mut Database::new(), &[], ExecOptions::default())?;
/// assert!(matches!(rows.as_slice(), [a, b] if matches!(a[0], Datum::Int8(10)) && matches!(b[0], Datum::Int8(20))));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn execute(
    plan: &Plan,
    catalog: &dyn Catalog,
    db: &mut Database,
    params: &[Datum],
    options: ExecOptions,
) -> Result<Vec<Vec<Datum>>, EvalError> {
    let mut working = db.clone();
    let rows = Interp::new(catalog, &mut working, params, options)
        .rel(&plan.root, &[])?
        .rows;
    *db = working;
    Ok(rows)
}

/// Evaluate a constant expression (no column references or parameters).
///
/// Used by constant folding; errors are returned rather than raised, so
/// the optimizer can leave an expression that would fail for run time.
pub(crate) fn eval_constant(
    expr: &crate::expr::Expr,
    catalog: &dyn Catalog,
) -> Result<Datum, EvalError> {
    let mut db = Database::new();
    Interp::new(catalog, &mut db, &[], ExecOptions::default()).eval(expr, &[])
}

/// Evaluates expressions on individual rows with the reference semantics.
///
/// The executor uses it wherever it needs row-at-a-time evaluation (filters
/// pushed into storage, and expressions until vectorized kernels cover
/// them), so there is one implementation of the semantics (ADR-0023).
pub struct RowEvaluator<'a> {
    /// The catalog, for function definitions.
    catalog: &'a dyn Catalog,
    /// Parameter values, `$1` first.
    params: Vec<Datum>,
    /// Settings such as the transaction time.
    options: ExecOptions,
    /// An empty database: expressions read no tables.
    db: Database,
}

impl<'a> RowEvaluator<'a> {
    /// Construct an evaluator.
    #[must_use]
    pub fn new(catalog: &'a dyn Catalog, params: Vec<Datum>, options: ExecOptions) -> Self {
        Self {
            catalog,
            params,
            options,
            db: Database::new(),
        }
    }

    /// Evaluate `expr` on `row`, whose values are the columns `layout` names, in order.
    ///
    /// # Errors
    ///
    /// Returns an [`EvalError`] for errors Postgres would raise, or
    /// [`EvalError::Unsupported`] if `expr` references a column not in `layout`.
    pub fn eval(
        &mut self,
        expr: &crate::expr::Expr,
        layout: &[(crate::names::BindingId, crate::names::ColumnId)],
        row: &[Datum],
    ) -> Result<Datum, EvalError> {
        let frame = super::eval::Frame { layout, row };
        let mut interp = Interp::new(self.catalog, &mut self.db, &self.params, self.options);
        let result = interp.eval(expr, &[frame]);
        // Continue the random sequence on the next row rather than repeating it.
        self.options.seed = interp.rng;
        result
    }
}
