//! Phase 3: predicate pushdown under the barrier rules (ADR-0014, ADR-0018).
//!
//! Filter conjuncts move as close to the scans as their columns allow:
//! through projections (by substituting the projected expressions), sorts,
//! `DISTINCT`, the outer side of applies, the sides of joins (only the
//! preserved side of an outer join; conjuncts over both sides of an inner
//! join become join conditions), aggregate and nest grouping keys, both
//! inputs of set operations, and the input of `UNNEST`. Join conditions
//! that reference one side move into it where the join kind allows.
//!
//! A conjunct moves below a `PolicyFilter` only if it is leakproof, so
//! rows the policy hides never reach code that could reveal them. Nothing
//! moves below a `WriteCheck`, and volatile conjuncts stay where they are.

use std::collections::HashSet;

use super::util::{
    Types, bindings_of, conjoin, conjuncts, leakproof, output_columns, produced, rewrite_expr,
    take, volatile,
};
use crate::catalog::Catalog;
use crate::expr::Expr;
use crate::names::BindingId;
use crate::rel::{JoinKind, Rel};

/// State of the pushdown pass.
pub(crate) struct Pushdown<'a> {
    /// The catalog, for function properties.
    pub(crate) catalog: &'a dyn Catalog,
    /// Column types, for leakproofness of operators.
    pub(crate) types: Types,
}

/// Wrap `rel` in a filter of `preds`, if there are any.
fn filtered(rel: Rel, preds: Vec<Expr>) -> Rel {
    if preds.is_empty() {
        rel
    } else {
        Rel::Filter {
            input: Box::new(rel),
            predicate: conjoin(preds),
        }
    }
}

/// Return `true` if `expr` references none of `bindings`.
fn avoids(expr: &Expr, bindings: &HashSet<BindingId>) -> bool {
    bindings_of(expr).is_disjoint(bindings)
}

/// Replace references to `binding`'s columns by `columns[i]`; `None` if a column has no replacement.
fn substitute(expr: &Expr, binding: BindingId, columns: &[Expr]) -> Option<Expr> {
    let mut out = expr.clone();
    let missing = std::cell::Cell::new(false);
    rewrite_expr(&mut out, &|b, c| {
        if b != binding {
            return None;
        }
        let replacement = usize::try_from(c.get())
            .ok()
            .and_then(|i| columns.get(i))
            .cloned();
        if replacement.is_none() {
            missing.set(true);
        }
        replacement
    });
    (!missing.get()).then_some(out)
}

impl Pushdown<'_> {
    /// Optimize `rel`: push every filter in it as far down as allowed.
    pub(crate) fn run(&self, rel: Rel) -> Rel {
        match rel {
            Rel::Filter { input, predicate } => self.push(*input, conjuncts(predicate)),
            mut other => {
                for input in other.inputs_mut() {
                    let child = take(input);
                    *input = self.run(child);
                }
                other
            }
        }
    }

    /// Push `preds` into `rel`, returning `rel` (optimized) with them applied at or below its root.
    #[allow(clippy::too_many_lines)]
    fn push(&self, rel: Rel, preds: Vec<Expr>) -> Rel {
        let (preds, pinned): (Vec<Expr>, Vec<Expr>) =
            preds.into_iter().partition(|p| !volatile(p, self.catalog));
        if preds.is_empty() {
            return filtered(self.run(rel), pinned);
        }
        let rel = match rel {
            Rel::Filter { input, predicate } => {
                // The lower filter's own conjuncts go first, keeping evaluation order.
                let mut all = conjuncts(predicate);
                all.extend(preds);
                return filtered(self.push(*input, all), pinned);
            }
            Rel::PolicyFilter { input, residual } => {
                let (down, up): (Vec<Expr>, Vec<Expr>) = preds
                    .into_iter()
                    .partition(|p| leakproof(p, &self.types, self.catalog));
                filtered(
                    Rel::PolicyFilter {
                        input: Box::new(self.push(*input, down)),
                        residual,
                    },
                    up,
                )
            }
            Rel::Project {
                input,
                exprs,
                binding: Some(binding),
            } => {
                let columns: Vec<Expr> = exprs.iter().map(|e| e.expr.clone()).collect();
                let inlinable = !columns.iter().any(|c| volatile(c, self.catalog));
                let (mut down, mut up) = (Vec::new(), Vec::new());
                for p in preds {
                    match substitute(&p, binding, &columns).filter(|_| inlinable) {
                        Some(inlined) => down.push(inlined),
                        None => up.push(p),
                    }
                }
                filtered(
                    Rel::Project {
                        input: Box::new(self.push(*input, down)),
                        exprs,
                        binding: Some(binding),
                    },
                    up,
                )
            }
            Rel::Sort { input, keys } => Rel::Sort {
                input: Box::new(self.push(*input, preds)),
                keys,
            },
            Rel::Distinct { input } => Rel::Distinct {
                input: Box::new(self.push(*input, preds)),
            },
            Rel::Join {
                kind,
                left,
                right,
                on,
            } => {
                let (lb, rb) = (produced(&left), produced(&right));
                let (mut to_left, mut to_right, mut to_on, mut up) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for p in preds {
                    let (touches_l, touches_r) = (!avoids(&p, &lb), !avoids(&p, &rb));
                    match kind {
                        JoinKind::Inner if !touches_r => to_left.push(p),
                        JoinKind::Inner if !touches_l => to_right.push(p),
                        JoinKind::Inner => to_on.push(p),
                        JoinKind::Left | JoinKind::Semi | JoinKind::Anti if !touches_r => {
                            to_left.push(p)
                        }
                        JoinKind::Right if !touches_l => to_right.push(p),
                        _ => up.push(p),
                    }
                }
                let mut kept = Vec::new();
                for c in conjuncts(on) {
                    if volatile(&c, self.catalog) {
                        kept.push(c);
                        continue;
                    }
                    let (touches_l, touches_r) = (!avoids(&c, &lb), !avoids(&c, &rb));
                    match kind {
                        JoinKind::Inner if !touches_r => to_left.push(c),
                        JoinKind::Inner if !touches_l => to_right.push(c),
                        JoinKind::Semi if !touches_r => to_left.push(c),
                        JoinKind::Left | JoinKind::Semi | JoinKind::Anti if !touches_l => {
                            to_right.push(c)
                        }
                        JoinKind::Right if !touches_r => to_left.push(c),
                        _ => kept.push(c),
                    }
                }
                kept.extend(to_on);
                filtered(
                    Rel::Join {
                        kind,
                        left: Box::new(self.push(*left, to_left)),
                        right: Box::new(self.push(*right, to_right)),
                        on: conjoin(kept),
                    },
                    up,
                )
            }
            Rel::Apply { kind, outer, inner } => {
                let ib = produced(&inner);
                let (down, up): (Vec<Expr>, Vec<Expr>) =
                    preds.into_iter().partition(|p| avoids(p, &ib));
                filtered(
                    Rel::Apply {
                        kind,
                        outer: Box::new(self.push(*outer, down)),
                        inner: Box::new(self.run(*inner)),
                    },
                    up,
                )
            }
            Rel::Aggregate {
                input,
                keys,
                aggs,
                binding: Some(binding),
            } if !keys.is_empty() => {
                let key_exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
                let inlinable = !key_exprs.iter().any(|k| volatile(k, self.catalog));
                let (mut down, mut up) = (Vec::new(), Vec::new());
                for p in preds {
                    match substitute(&p, binding, &key_exprs).filter(|_| inlinable) {
                        Some(inlined) => down.push(inlined),
                        None => up.push(p),
                    }
                }
                filtered(
                    Rel::Aggregate {
                        input: Box::new(self.push(*input, down)),
                        keys,
                        aggs,
                        binding: Some(binding),
                    },
                    up,
                )
            }
            Rel::Nest {
                input,
                keys,
                fields,
                name,
                binding: Some(binding),
            } if !keys.is_empty() => {
                let key_exprs: Vec<Expr> = keys.iter().map(|k| k.expr.clone()).collect();
                let (mut down, mut up) = (Vec::new(), Vec::new());
                for p in preds {
                    match substitute(&p, binding, &key_exprs) {
                        Some(inlined) => down.push(inlined),
                        None => up.push(p),
                    }
                }
                filtered(
                    Rel::Nest {
                        input: Box::new(self.push(*input, down)),
                        keys,
                        fields,
                        name,
                        binding: Some(binding),
                    },
                    up,
                )
            }
            Rel::SetOp {
                op,
                all,
                left,
                right,
                binding: Some(binding),
            } => {
                let side_columns = |side: &Rel| -> Vec<Expr> {
                    output_columns(side, self.catalog)
                        .into_iter()
                        .map(|(b, c)| super::util::column(b, c))
                        .collect()
                };
                let (lc, rc) = (side_columns(&left), side_columns(&right));
                let (mut to_left, mut to_right, mut up) = (Vec::new(), Vec::new(), Vec::new());
                for p in preds {
                    match (substitute(&p, binding, &lc), substitute(&p, binding, &rc)) {
                        (Some(l), Some(r)) => {
                            to_left.push(l);
                            to_right.push(r);
                        }
                        _ => up.push(p),
                    }
                }
                filtered(
                    Rel::SetOp {
                        op,
                        all,
                        left: Box::new(self.push(*left, to_left)),
                        right: Box::new(self.push(*right, to_right)),
                        binding: Some(binding),
                    },
                    up,
                )
            }
            Rel::Unnest {
                input,
                expr,
                columns,
                binding: Some(binding),
            } => {
                let own: HashSet<BindingId> = std::iter::once(binding).collect();
                let (down, up): (Vec<Expr>, Vec<Expr>) =
                    preds.into_iter().partition(|p| avoids(p, &own));
                filtered(
                    Rel::Unnest {
                        input: Box::new(self.push(*input, down)),
                        expr,
                        columns,
                        binding: Some(binding),
                    },
                    up,
                )
            }
            other => filtered(self.run(other), preds),
        };
        filtered(rel, pinned)
    }
}
