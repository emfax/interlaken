//! `EXPLAIN` and `EXPLAIN ANALYZE` (ADR-0028, ADR-0025).
//!
//! `EXPLAIN` prints the physical plan one operator per line, indented under
//! its parent. Each line names the algorithm and what was pushed into each
//! storage scan, and shows where every `PolicyFilter` sits. `EXPLAIN
//! ANALYZE` runs the query, discarding its rows, and appends each
//! operator's metrics.
//!
//! Policy logic is shown only to principals allowed to read policy
//! definitions ([`ExplainOptions::show_policies`]). Everyone else sees
//! `PolicyFilter(table)` with no residual. The subplans security lowering
//! adds carry a marker (their scans' aliases and literal relations'
//! column names use the reserved policy prefix), so each is collapsed to
//! one line wherever the optimizer put it beneath a policy filter or write
//! check, and the conditions joining them are hidden. Beneath a policy
//! filter every expression is hidden. A governed scan hides its policy
//! conjuncts and key values, and a `WriteCheck` hides its check. A
//! projection that masks columns (ADR-0032) hides each mask, and its input
//! is treated like a write check's: subplans the mask's condition added
//! are collapsed. The plan's shape stays visible, but its policy logic
//! does not.

use std::fmt::Write as _;
use std::ops::Bound;

use futures::TryStreamExt;
use interlaken_ir::text::print_expr;
use interlaken_ir::{Expr, JoinKind, SetOpKind, SortKey};

use crate::access::{Access, KeyBounds};
use crate::error::ExecError;
use crate::exec::{ExecContext, execute};
use crate::lookup::LookupKey;
use crate::metrics::{Metrics, NodeStats};
use crate::physical::{Op, Physical};

/// What `EXPLAIN` may show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExplainOptions {
    /// Show policy residuals, write checks and the subplans policies add;
    /// only for principals allowed to read policy definitions.
    pub show_policies: bool,
}

/// What hides the policy parts of a plan.
const HIDDEN: &str = "<policy>";

/// Render the physical plan rooted at `node`.
#[must_use]
pub fn explain(node: &Physical, options: ExplainOptions) -> String {
    let mut out = String::new();
    Renderer {
        options,
        metrics: None,
        out: &mut out,
    }
    .node(node, 0, Scope::Open);
    out
}

/// Run the plan rooted at `node`, discarding its rows, and render it with each operator's metrics.
///
/// # Errors
///
/// Returns the error the query raises, if it fails.
pub async fn explain_analyze(
    node: &Physical,
    ctx: &ExecContext,
    options: ExplainOptions,
) -> Result<String, ExecError> {
    let metrics = Metrics::new(node.size());
    let ctx = ctx.clone().with_metrics(metrics.clone());
    execute(node, &ctx)
        .try_for_each(|_| async { Ok(()) })
        .await?;
    drop(ctx);
    let mut out = String::new();
    Renderer {
        options,
        metrics: Some(&metrics),
        out: &mut out,
    }
    .node(node, 0, Scope::Open);
    Ok(out)
}

/// Writes plan lines.
struct Renderer<'a> {
    /// What may be shown.
    options: ExplainOptions,
    /// Metrics to append, for `EXPLAIN ANALYZE`.
    metrics: Option<&'a Metrics>,
    /// The output.
    out: &'a mut String,
}

/// Render an expression.
fn expr(e: &Expr) -> String {
    print_expr(e)
}

/// Render a list of expressions.
fn list(items: &[Expr]) -> String {
    items.iter().map(expr).collect::<Vec<_>>().join(", ")
}

/// Render a sort key.
fn sort_key(k: &SortKey) -> String {
    let direction = if k.descending { "desc" } else { "asc" };
    format!("{} {direction}", expr(&k.expr))
}

/// Render key bounds, or `<policy>` when they are hidden.
fn bounds(b: &KeyBounds, hidden: bool) -> String {
    if hidden {
        return HIDDEN.to_owned();
    }
    let mut parts = Vec::new();
    if !b.prefix.is_empty() {
        parts.push(format!("= ({})", list(&b.prefix)));
    }
    match &b.lower {
        Bound::Included(e) => parts.push(format!(">= {}", expr(e))),
        Bound::Excluded(e) => parts.push(format!("> {}", expr(e))),
        Bound::Unbounded => {}
    }
    match &b.upper {
        Bound::Included(e) => parts.push(format!("<= {}", expr(e))),
        Bound::Excluded(e) => parts.push(format!("< {}", expr(e))),
        Bound::Unbounded => {}
    }
    parts.join(" and ")
}

/// Render a millisecond duration.
fn millis(stats: &NodeStats) -> String {
    format!("{:.3}ms", stats.elapsed.as_secs_f64() * 1000.0)
}

impl Renderer<'_> {
    /// Write one operator's line (with metrics, if any) at `depth`.
    fn line(&mut self, node: &Physical, depth: usize, text: &str) {
        let _ = write!(self.out, "{}{text}", "  ".repeat(depth));
        if let Some(stats) = self
            .metrics
            .and_then(|m| m.node(node.id))
            .map(|m| m.snapshot())
        {
            let _ = write!(
                self.out,
                "  (rows={} batches={} loops={} time={}",
                stats.rows,
                stats.batches,
                stats.loops,
                millis(&stats)
            );
            if stats.storage_calls > 0 {
                let _ = write!(
                    self.out,
                    " storage_calls={} bytes_read={}",
                    stats.storage_calls, stats.bytes_read
                );
            }
            if stats.peak_memory > 0 {
                let _ = write!(self.out, " peak_memory={}", stats.peak_memory);
            }
            self.out.push(')');
        }
        self.out.push('\n');
    }

    /// Write the line for a subplan a policy added, in place of its operators.
    fn hidden_subplan(&mut self, depth: usize) {
        let _ = writeln!(self.out, "{}{HIDDEN} subplan", "  ".repeat(depth));
    }

    /// Return whether policy details are hidden.
    fn hiding(&self) -> bool {
        !self.options.show_policies
    }

    /// Render `e`, or `<policy>` if expressions in `scope` are policy logic to hide.
    fn shown(&self, scope: Scope, e: &Expr) -> String {
        if self.hiding() && scope == Scope::Policy {
            HIDDEN.to_owned()
        } else {
            expr(e)
        }
    }

    /// Render a list of expressions, or `<policy>` as for [`Renderer::shown`].
    fn shown_list(&self, scope: Scope, items: &[Expr]) -> String {
        if self.hiding() && scope == Scope::Policy && !items.is_empty() {
            HIDDEN.to_owned()
        } else {
            list(items)
        }
    }

    /// Write `node` and its inputs at `depth`, in `scope`.
    ///
    /// Under a policy filter or write check, a subplan security lowering
    /// added (one whose every leaf carries the policy marker) is written as
    /// one hidden line, and so is the condition of a join with such a side.
    /// Under a policy filter, every other expression is policy logic too,
    /// since the filter's input is the governed scan and what lowering
    /// joined to it.
    #[allow(clippy::too_many_lines)]
    fn node(&mut self, node: &Physical, depth: usize, scope: Scope) {
        let hide = self.hiding();
        if hide && scope != Scope::Open && policy_subplan(node) {
            self.hidden_subplan(depth);
            return;
        }
        match &node.op {
            Op::TableScan {
                table,
                access,
                pushed,
                policy,
                secured,
                ..
            } => {
                let name = table.name.joined();
                let hidden = hide && *secured;
                let mut text = match access {
                    Access::Full => format!("TableScan {name}"),
                    Access::Primary(b) => {
                        format!("TableScan {name} primary key {}", bounds(b, hidden))
                    }
                    Access::Index {
                        name: index,
                        bounds: b,
                    } => {
                        format!("IndexScan {name} using {index} {}", bounds(b, hidden))
                    }
                };
                if let Some(p) = pushed {
                    let _ = write!(text, " pushed: {}", expr(p));
                }
                if let Some(p) = policy {
                    let shown = if hide { HIDDEN.to_owned() } else { expr(p) };
                    let _ = write!(text, " policy: {shown}");
                }
                self.line(node, depth, &text);
            }
            Op::Filter { input, predicate } => {
                let text = format!("Filter {}", self.shown(scope, predicate));
                self.line(node, depth, &text);
                self.node(input, depth + 1, scope);
            }
            Op::PolicyFilter {
                input,
                residual,
                table,
            } => {
                let text = if hide {
                    format!("PolicyFilter({table})")
                } else {
                    format!("PolicyFilter({table}) {}", expr(residual))
                };
                self.line(node, depth, &text);
                self.node(input, depth + 1, Scope::Policy);
            }
            Op::WriteCheck {
                input,
                check,
                table,
            } => {
                let text = if hide {
                    format!("WriteCheck({table})")
                } else {
                    format!("WriteCheck({table}) {}", expr(check))
                };
                self.line(node, depth, &text);
                self.node(input, depth + 1, scope.max(Scope::Check));
            }
            Op::Project { input, exprs } => {
                let masked = exprs.iter().any(is_mask);
                let text = if hide && masked {
                    let items: Vec<String> = exprs
                        .iter()
                        .map(|e| {
                            if is_mask(e) {
                                format!("{HIDDEN} mask")
                            } else {
                                self.shown(scope, e)
                            }
                        })
                        .collect();
                    format!("Project [{}]", items.join(", "))
                } else {
                    format!("Project {}", self.shown_list(scope, exprs))
                };
                self.line(node, depth, &text);
                let inner = if masked {
                    scope.max(Scope::Check)
                } else {
                    scope
                };
                self.node(input, depth + 1, inner);
            }
            Op::Limit {
                input,
                limit,
                offset,
            } => {
                let mut text = "Limit".to_owned();
                if let Some(l) = limit {
                    let _ = write!(text, " count: {}", self.shown(scope, l));
                }
                if let Some(o) = offset {
                    let _ = write!(text, " offset: {}", self.shown(scope, o));
                }
                self.line(node, depth, &text);
                self.node(input, depth + 1, scope);
            }
            Op::Join {
                kind,
                left,
                right,
                keys,
                residual,
            } => {
                let algorithm = if keys.is_empty() {
                    "NestedLoopJoin"
                } else {
                    "HashJoin"
                };
                let mut text = format!("{algorithm} {}", join_kind(*kind));
                let policy_side =
                    scope != Scope::Open && (policy_subplan(left) || policy_subplan(right));
                if hide && (scope == Scope::Policy || policy_side) {
                    let _ = write!(text, " on {HIDDEN}");
                } else {
                    if !keys.is_empty() {
                        let keys: Vec<String> = keys
                            .iter()
                            .map(|(l, r)| format!("{} = {}", expr(l), expr(r)))
                            .collect();
                        let _ = write!(text, " keys: {}", keys.join(", "));
                    }
                    if let Some(r) = residual {
                        let _ = write!(text, " residual: {}", expr(r));
                    }
                }
                self.line(node, depth, &text);
                self.node(left, depth + 1, scope);
                self.node(right, depth + 1, scope);
            }
            Op::IndexLookup {
                kind,
                left,
                right,
                key,
                keys,
                residual,
                ..
            } => {
                let using = match key {
                    LookupKey::Primary => "primary key".to_owned(),
                    LookupKey::Index(name) => name.to_string(),
                };
                let mut text = format!("IndexLookup {} using {using}", join_kind(*kind));
                let policy_side =
                    scope != Scope::Open && (policy_subplan(left) || policy_subplan(right));
                if hide && (scope == Scope::Policy || policy_side) {
                    let _ = write!(text, " on {HIDDEN}");
                } else {
                    let keys: Vec<String> = keys
                        .iter()
                        .map(|(l, r)| format!("{} = {}", expr(l), expr(r)))
                        .collect();
                    let _ = write!(text, " keys: {}", keys.join(", "));
                    if let Some(r) = residual {
                        let _ = write!(text, " residual: {}", expr(r));
                    }
                }
                self.line(node, depth, &text);
                self.node(left, depth + 1, scope);
                self.node(right, depth + 1, scope);
            }
            Op::Apply { kind, outer, inner } => {
                let kind = format!("{kind:?}").to_lowercase();
                self.line(node, depth, &format!("Apply {kind}"));
                self.node(outer, depth + 1, scope);
                self.node(inner, depth + 1, scope);
            }
            Op::Aggregate { input, keys, aggs } => {
                let aggs: Vec<String> = aggs
                    .iter()
                    .map(|a| {
                        let distinct = if a.distinct { "distinct " } else { "" };
                        let mut s = format!(
                            "{}({distinct}{})",
                            a.def.name,
                            self.shown_list(scope, &a.args)
                        );
                        if let Some(f) = &a.filter {
                            let _ = write!(s, " filter {}", self.shown(scope, f));
                        }
                        s
                    })
                    .collect();
                let text = format!(
                    "HashAggregate keys: [{}] aggs: [{}]",
                    self.shown_list(scope, keys),
                    aggs.join(", ")
                );
                self.line(node, depth, &text);
                self.node(input, depth + 1, scope);
            }
            Op::Distinct { input } => {
                self.line(node, depth, "Distinct");
                self.node(input, depth + 1, scope);
            }
            Op::Sort { input, keys, fetch } => {
                let keys: Vec<String> = if hide && scope == Scope::Policy {
                    vec![HIDDEN.to_owned()]
                } else {
                    keys.iter().map(sort_key).collect()
                };
                let name = if fetch.is_some() { "TopN" } else { "Sort" };
                self.line(node, depth, &format!("{name} {}", keys.join(", ")));
                self.node(input, depth + 1, scope);
            }
            Op::SetOp {
                op,
                all,
                left,
                right,
            } => {
                let name = match op {
                    SetOpKind::Union => "Union",
                    SetOpKind::Intersect => "Intersect",
                    SetOpKind::Except => "Except",
                };
                let all = if *all { " all" } else { "" };
                self.line(node, depth, &format!("{name}{all}"));
                self.node(left, depth + 1, scope);
                self.node(right, depth + 1, scope);
            }
            Op::Values { rows, .. } => {
                self.line(node, depth, &format!("Values rows: {}", rows.len()));
            }
            Op::Unnest { input, expr: e, .. } => {
                let text = format!("Unnest {}", self.shown(scope, e));
                self.line(node, depth, &text);
                self.node(input, depth + 1, scope);
            }
            Op::Nest {
                input,
                keys,
                fields,
            } => {
                let text = format!(
                    "Nest keys: [{}] fields: [{}]",
                    self.shown_list(scope, keys),
                    self.shown_list(scope, fields)
                );
                self.line(node, depth, &text);
                self.node(input, depth + 1, scope);
            }
            Op::Insert { dml, .. } => {
                self.line(
                    node,
                    depth,
                    &format!("Insert into {}{}", dml.table.name.joined(), maintains(dml)),
                );
                self.node(&dml.input, depth + 1, scope);
            }
            Op::Update {
                dml, assignments, ..
            } => {
                let sets: Vec<String> = assignments
                    .iter()
                    .map(|(c, e)| {
                        let column = dml
                            .table
                            .columns
                            .get(*c)
                            .map_or_else(|| c.to_string(), |c| c.name.to_string());
                        format!("{column} = {}", expr(e))
                    })
                    .collect();
                self.line(
                    node,
                    depth,
                    &format!(
                        "Update {} set {}{}",
                        dml.table.name.joined(),
                        sets.join(", "),
                        maintains(dml)
                    ),
                );
                self.node(&dml.input, depth + 1, scope);
            }
            Op::Delete { dml, .. } => {
                self.line(
                    node,
                    depth,
                    &format!("Delete from {}{}", dml.table.name.joined(), maintains(dml)),
                );
                self.node(&dml.input, depth + 1, scope);
            }
        }
    }
}

/// Return whether `e` contains a column mask security lowering added (ADR-0032).
fn is_mask(e: &Expr) -> bool {
    match e {
        Expr::Call { func, args } => {
            /// The IDs of the `policy_mask` overloads, which built-ins fix.
            static MASKS: std::sync::OnceLock<Vec<interlaken_ir::FunctionId>> =
                std::sync::OnceLock::new();
            let masks = MASKS.get_or_init(|| {
                interlaken_ir::catalog::builtin_functions()
                    .into_iter()
                    .filter(|f| f.name.to_string() == interlaken_ir::security::POLICY_MASK)
                    .map(|f| f.id)
                    .collect()
            });
            let named = match func {
                interlaken_ir::FunctionRef::Named(name) => {
                    name.to_string() == interlaken_ir::security::POLICY_MASK
                }
                interlaken_ir::FunctionRef::Id(id) => masks.contains(id),
            };
            named || args.iter().any(is_mask)
        }
        _ => false,
    }
}

/// Render the closure tables a DML operator maintains (ADR-0017) and the
/// foreign keys it checks (ADR-0035), if any.
fn maintains(dml: &crate::physical::Dml) -> String {
    let closures = dml.closure.as_ref().map_or_else(String::new, |plan| {
        format!(" maintaining closures: {}", plan.table_names().join(", "))
    });
    let references = dml.references.as_ref().map_or_else(String::new, |plan| {
        let cascades = plan.cascades();
        let cascading = if cascades.is_empty() {
            String::new()
        } else {
            format!(" cascading to: {}", cascades.join(", "))
        };
        format!(
            " checking foreign keys: {}{cascading}",
            plan.names().join(", ")
        )
    });
    closures + &references
}

/// Where in the plan an operator sits, for hiding policy logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    /// Outside any policy filter or write check: nothing is policy logic.
    Open,
    /// Beneath a write check: subplans lowering added are policy logic.
    Check,
    /// Beneath a policy filter: all expressions are policy logic.
    Policy,
}

/// Return whether security lowering added the whole subplan: every leaf
/// carries the policy marker.
fn policy_subplan(node: &Physical) -> bool {
    match &node.op {
        Op::TableScan { policy_added, .. } | Op::Values { policy_added, .. } => *policy_added,
        op => {
            let inputs = op.inputs();
            !inputs.is_empty() && inputs.into_iter().all(policy_subplan)
        }
    }
}

/// Return the name of a join kind.
fn join_kind(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "inner",
        JoinKind::Left => "left",
        JoinKind::Right => "right",
        JoinKind::Full => "full",
        JoinKind::Semi => "semi",
        JoinKind::Anti => "anti",
    }
}
