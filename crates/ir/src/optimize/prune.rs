//! Phase 4: projection pruning (ADR-0014).
//!
//! Outputs nothing reads are removed from projections and aggregates
//! (aggregate calls only; grouping keys define the groups). Operators whose
//! output is consumed by position or as whole rows are left alone: the
//! plan's output, set operation and insert inputs, and `DISTINCT` inputs,
//! along with everything that passes those columns through. Which columns a
//! scan reads is derived from the remaining references at physical planning.

use std::collections::{HashMap, HashSet};

use super::util::{col_id, rewrite_rel};
use crate::expr::Expr;
use crate::names::{BindingId, ColumnId, ColumnRef};
use crate::rel::{ApplyKind, JoinKind, Rel};

/// Collect every column reference in `rel` and below.
pub(crate) fn referenced(rel: &Rel, out: &mut HashSet<(BindingId, ColumnId)>) {
    /// Collect references in one expression.
    fn walk(e: &Expr, out: &mut HashSet<(BindingId, ColumnId)>) {
        if let Expr::Column(ColumnRef::Bound { binding, column }) = e {
            out.insert((*binding, *column));
        }
        e.children().into_iter().for_each(|c| walk(c, out));
    }
    rel.exprs().into_iter().for_each(|e| walk(e, out));
    rel.inputs().into_iter().for_each(|i| referenced(i, out));
}

/// Collect the bindings whose outputs are consumed by position, given whether `rel`'s are.
pub(crate) fn positional(rel: &Rel, consumed: bool, out: &mut HashSet<BindingId>) {
    if consumed {
        out.extend(rel.bindings());
    }
    match rel {
        Rel::SetOp { left, right, .. } => {
            positional(left, true, out);
            positional(right, true, out);
        }
        Rel::Insert { input, .. } | Rel::Distinct { input } => positional(input, true, out),
        Rel::Filter { input, .. }
        | Rel::PolicyFilter { input, .. }
        | Rel::WriteCheck { input, .. }
        | Rel::Sort { input, .. }
        | Rel::Limit { input, .. }
        | Rel::Unnest { input, .. } => positional(input, consumed, out),
        Rel::Join {
            kind, left, right, ..
        } => {
            positional(left, consumed, out);
            positional(
                right,
                consumed && !matches!(kind, JoinKind::Semi | JoinKind::Anti),
                out,
            );
        }
        Rel::Apply { kind, outer, inner } => {
            positional(outer, consumed, out);
            positional(
                inner,
                consumed && !matches!(kind, ApplyKind::Semi | ApplyKind::Anti),
                out,
            );
        }
        other => other
            .inputs()
            .into_iter()
            .for_each(|i| positional(i, false, out)),
    }
}

/// Remove unread outputs; return the old-to-new column positions of each changed binding.
fn prune_once(
    rel: &mut Rel,
    used: &HashSet<(BindingId, ColumnId)>,
    keep: &HashSet<BindingId>,
    remaps: &mut HashMap<BindingId, Vec<Option<ColumnId>>>,
) {
    for input in rel.inputs_mut() {
        prune_once(input, used, keep, remaps);
    }
    match rel {
        Rel::Project {
            exprs,
            binding: Some(b),
            ..
        } if !keep.contains(b) => {
            let live: Vec<bool> = (0..exprs.len())
                .map(|i| used.contains(&(*b, col_id(i))))
                .collect();
            if live.iter().all(|l| *l) {
                return;
            }
            let mut next = 0;
            let map = live
                .iter()
                .map(|&l| {
                    l.then(|| {
                        next += 1;
                        col_id(next - 1)
                    })
                })
                .collect();
            let mut i = 0;
            exprs.retain(|_| {
                i += 1;
                live[i - 1]
            });
            remaps.insert(*b, map);
        }
        Rel::Aggregate {
            keys,
            aggs,
            binding: Some(b),
            ..
        } if !keep.contains(b) => {
            let n = keys.len();
            let live: Vec<bool> = (0..aggs.len())
                .map(|j| used.contains(&(*b, col_id(n + j))))
                .collect();
            if live.iter().all(|l| *l) {
                return;
            }
            let mut next = n;
            let map = (0..n)
                .map(|i| Some(col_id(i)))
                .chain(live.iter().map(|&l| {
                    l.then(|| {
                        next += 1;
                        col_id(next - 1)
                    })
                }))
                .collect();
            let mut j = 0;
            aggs.retain(|_| {
                j += 1;
                live[j - 1]
            });
            remaps.insert(*b, map);
        }
        _ => {}
    }
}

/// Prune unread projection and aggregate outputs until nothing changes.
pub(crate) fn prune(root: &mut Rel) {
    loop {
        let mut used = HashSet::new();
        referenced(root, &mut used);
        let mut keep = HashSet::new();
        positional(root, true, &mut keep);
        let mut remaps = HashMap::new();
        prune_once(root, &used, &keep, &mut remaps);
        if remaps.is_empty() {
            return;
        }
        rewrite_rel(root, &|b, c| {
            let map = remaps.get(&b)?;
            let new = map.get(usize::try_from(c.get()).ok()?)?.as_ref()?;
            Some(Expr::Column(ColumnRef::bound(b, *new)))
        });
    }
}
