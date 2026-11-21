//! The optimizer's entry point.

use super::decorrelate::decorrelate;
use super::order::order;
use super::prune::prune;
use super::pushdown::Pushdown;
use super::simplify::simplify;
use super::util::take;
use crate::catalog::Catalog;
use crate::rel::Plan;
use crate::verify::column_types;

/// Optimize a resolved, verified plan by rule-based rewriting in fixed phases:
/// simplification, decorrelation, predicate pushdown under the barrier rules,
/// projection pruning, and limit pushdown with sort elimination.
///
/// The result means the same as the input (the reference interpreter
/// returns the same rows for both, as a bag) and verifies under the same
/// options. It may raise an error the input would not have raised, as
/// Postgres may, when a moved predicate meets rows earlier operators used
/// to discard.
///
/// # Examples
///
/// ```
/// use interlaken_ir::optimize::optimize;
/// use interlaken_ir::text::{parse_catalog, parse_plan, print_plan};
///
/// let catalog = parse_catalog(
///     "(catalog (builtins) (table t :id 100 (column x int8) (column y (option int8))))",
/// )?;
/// let plan = parse_plan(
///     "(plan (filter (project (get (tid 100) :bind 1) (as #1.0 x) :bind 2) (and true (> #2.0 (+ 1 2))))
///        :schema-version 1)",
/// )?;
/// assert_eq!(
///     print_plan(&optimize(&plan, &catalog)),
///     "(plan :schema-version 1\n  (project (filter (get (tid 100) :bind 1) (> #1.0 3)) (as #1.0 x) :bind 2))"
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn optimize(plan: &Plan, catalog: &dyn Catalog) -> Plan {
    let mut plan = plan.clone();
    let types = column_types(&plan, catalog);
    simplify(&mut plan.root, &types, catalog);
    decorrelate(&mut plan, catalog);
    let types = column_types(&plan, catalog);
    simplify(&mut plan.root, &types, catalog);
    let pushdown = Pushdown { catalog, types };
    plan.root = pushdown.run(take(&mut plan.root));
    simplify(&mut plan.root, &pushdown.types, catalog);
    prune(&mut plan.root);
    order(&mut plan.root, catalog);
    plan
}

/// Return `true` if `expr`, evaluated within `plan`, can run below a
/// security barrier: every function, operator and cast in it is leakproof
/// (ADR-0011, ADR-0018), judged from the catalog and the plan's column types.
#[must_use]
pub fn is_leakproof(expr: &crate::expr::Expr, plan: &Plan, catalog: &dyn Catalog) -> bool {
    super::util::leakproof(expr, &column_types(plan, catalog), catalog)
}
