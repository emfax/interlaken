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
//! adds beneath a policy filter (the inner side of each join or apply on
//! the way down to the governed rows) are collapsed to one line. A
//! governed scan hides its policy conjuncts and key values, and a
//! `WriteCheck` hides its check. The plan's shape stays visible, but its
//! policy logic does not.

use std::fmt::Write as _;
use std::ops::Bound;

use futures::TryStreamExt;
use interlaken_ir::text::print_expr;
use interlaken_ir::{Expr, JoinKind, SetOpKind, SortKey};

use crate::access::{Access, KeyBounds};
use crate::error::ExecError;
use crate::exec::{ExecContext, execute};
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
    .node(node, 0, false);
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
    .node(node, 0, false);
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

    /// Write `node` and its inputs at `depth`.
    ///
    /// `governed` is set on the path from a policy filter down to the rows
    /// it governs: joins and applies there have their inner side hidden.
    #[allow(clippy::too_many_lines)]
    fn node(&mut self, node: &Physical, depth: usize, governed: bool) {
        let hide = self.hiding();
        match &node.op {
            Op::TableScan {
                table,
                access,
                pushed,
                policy,
                secured,
                ..
            } => {
                let name = table.name.last();
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
                self.line(node, depth, &format!("Filter {}", expr(predicate)));
                self.node(input, depth + 1, governed);
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
                self.node(input, depth + 1, true);
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
                self.node(input, depth + 1, false);
            }
            Op::Project { input, exprs } => {
                self.line(node, depth, &format!("Project {}", list(exprs)));
                self.node(input, depth + 1, false);
            }
            Op::Limit {
                input,
                limit,
                offset,
            } => {
                let mut text = "Limit".to_owned();
                if let Some(l) = limit {
                    let _ = write!(text, " count: {}", expr(l));
                }
                if let Some(o) = offset {
                    let _ = write!(text, " offset: {}", expr(o));
                }
                self.line(node, depth, &text);
                self.node(input, depth + 1, false);
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
                if governed && hide {
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
                self.node(left, depth + 1, governed);
                if governed && hide {
                    self.hidden_subplan(depth + 1);
                } else {
                    self.node(right, depth + 1, false);
                }
            }
            Op::Apply { kind, outer, inner } => {
                let kind = format!("{kind:?}").to_lowercase();
                self.line(node, depth, &format!("Apply {kind}"));
                self.node(outer, depth + 1, governed);
                if governed && hide {
                    self.hidden_subplan(depth + 1);
                } else {
                    self.node(inner, depth + 1, false);
                }
            }
            Op::Aggregate { input, keys, aggs } => {
                let aggs: Vec<String> = aggs
                    .iter()
                    .map(|a| {
                        let distinct = if a.distinct { "distinct " } else { "" };
                        let mut s = format!("{}({distinct}{})", a.def.name, list(&a.args));
                        if let Some(f) = &a.filter {
                            let _ = write!(s, " filter {}", expr(f));
                        }
                        s
                    })
                    .collect();
                self.line(
                    node,
                    depth,
                    &format!(
                        "HashAggregate keys: [{}] aggs: [{}]",
                        list(keys),
                        aggs.join(", ")
                    ),
                );
                self.node(input, depth + 1, false);
            }
            Op::Distinct { input } => {
                self.line(node, depth, "Distinct");
                self.node(input, depth + 1, false);
            }
            Op::Sort { input, keys, fetch } => {
                let keys: Vec<String> = keys.iter().map(sort_key).collect();
                let name = if fetch.is_some() { "TopN" } else { "Sort" };
                self.line(node, depth, &format!("{name} {}", keys.join(", ")));
                self.node(input, depth + 1, false);
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
                self.node(left, depth + 1, false);
                self.node(right, depth + 1, false);
            }
            Op::Values { rows } => {
                self.line(node, depth, &format!("Values rows: {}", rows.len()));
            }
            Op::Unnest { input, expr: e, .. } => {
                self.line(node, depth, &format!("Unnest {}", expr(e)));
                self.node(input, depth + 1, governed);
            }
            Op::Nest {
                input,
                keys,
                fields,
            } => {
                self.line(
                    node,
                    depth,
                    &format!("Nest keys: [{}] fields: [{}]", list(keys), list(fields)),
                );
                self.node(input, depth + 1, false);
            }
            Op::Insert { dml, .. } => {
                self.line(
                    node,
                    depth,
                    &format!("Insert into {}", dml.table.name.last()),
                );
                self.node(&dml.input, depth + 1, false);
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
                    &format!("Update {} set {}", dml.table.name.last(), sets.join(", ")),
                );
                self.node(&dml.input, depth + 1, false);
            }
            Op::Delete { dml, .. } => {
                self.line(
                    node,
                    depth,
                    &format!("Delete from {}", dml.table.name.last()),
                );
                self.node(&dml.input, depth + 1, false);
            }
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
