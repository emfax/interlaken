//! Phase 2: decorrelation of `Apply` into joins (ADR-0012, ADR-0014).
//!
//! Rules, applied bottom-up so inner applies are handled first:
//!
//! - **Uncorrelated:** an apply whose inner side does not reference the
//!   outer side becomes a join on `true` (a single apply only if the inner
//!   side yields at most one row).
//! - **Correlated filters:** for semi, anti, inner and left applies,
//!   correlated conjuncts are pulled out of the inner side (through
//!   filters, projections, sorts and, where row counts do not matter,
//!   `DISTINCT`) into the join condition.
//! - **Scalar aggregates** (Neumann and Kemper): an apply over an
//!   aggregate without grouping keys, correlated only by equalities, becomes
//!   a left join to the same aggregate grouped by the correlated inner
//!   expressions. An outer row with no group gets NULLs from the join, which
//!   is right for every aggregate except `count`, whose references are
//!   wrapped in `coalesce(..., 0)` (the "count bug").
//! - **Nesting:** the same for `Nest` without keys (GraphQL children),
//!   with references wrapped in `coalesce(..., empty array)`.
//! - **EXISTS markers:** a left apply of one marker row becomes a left join
//!   to a `DISTINCT` projection of the correlated inner expressions.
//!
//! Applies that fit none of these (for example `NOT IN`, whose lowering
//! aggregates over outer columns) stay and run as nested loops.

use std::collections::{HashMap, HashSet};

use super::util::{
    Bindings, Types, bindings_of, col_id, column, conjoin, conjuncts, data_type, free, produced,
    rewrite_rel, take,
};
use crate::catalog::Catalog;
use crate::expr::{BinaryOp, Expr, NamedExpr};
use crate::names::{BindingId, ColumnId, FunctionRef};
use crate::rel::{ApplyKind, JoinKind, Rel};
use crate::typing;
use crate::value::Value;

/// State of one decorrelation pass.
pub(crate) struct Decorrelator<'a> {
    /// The catalog, for function names and types.
    pub(crate) catalog: &'a dyn Catalog,
    /// Column types of the plan as it was when the pass started.
    pub(crate) types: Types,
    /// Fresh bindings for rewritten operators.
    pub(crate) ids: Bindings,
    /// Replacements for references to bindings that rewrites removed.
    pub(crate) remaps: HashMap<BindingId, Vec<Expr>>,
    /// Set once a rewrite needs references remapped: until they are,
    /// other references to the removed binding are stale, so the pass
    /// makes no further rewrites.
    frozen: bool,
    /// Whether this pass rewrote anything.
    changed: bool,
}

/// Return `true` if `expr` references any of `bindings`.
fn touches(expr: &Expr, bindings: &HashSet<BindingId>) -> bool {
    !bindings_of(expr).is_disjoint(bindings)
}

/// Pull the conjuncts of `rel` that reference `outer` up out of it.
///
/// Returns the relation without them and the pulled conjuncts, rewritten
/// to reference only `rel`'s output columns and the outer side. Fails
/// (`None`) if `rel` depends on `outer` anywhere else. When `counts_matter`
/// is false the caller only cares whether rows exist, which allows pulling
/// through `DISTINCT`.
fn pull(rel: Rel, outer: &HashSet<BindingId>, counts_matter: bool) -> Option<(Rel, Vec<Expr>)> {
    if free(&rel).is_disjoint(outer) {
        return Some((rel, Vec::new()));
    }
    match rel {
        Rel::Filter { input, predicate } => {
            let (input, mut pulled) = pull(*input, outer, counts_matter)?;
            let (correlated, local): (Vec<Expr>, Vec<Expr>) = conjuncts(predicate)
                .into_iter()
                .partition(|c| touches(c, outer));
            pulled.extend(correlated);
            let rel = if local.is_empty() {
                input
            } else {
                Rel::Filter {
                    input: Box::new(input),
                    predicate: conjoin(local),
                }
            };
            Some((rel, pulled))
        }
        Rel::Project {
            input,
            mut exprs,
            binding,
        } => {
            if exprs.iter().any(|e| touches(&e.expr, outer)) {
                return None;
            }
            let (input, mut pulled) = pull(*input, outer, counts_matter)?;
            let binding_id = binding?;
            // Expose the input columns the pulled conjuncts need, after the existing outputs.
            let inside = produced(&input);
            let mut exposed: HashMap<(BindingId, ColumnId), ColumnId> = HashMap::new();
            for conjunct in &pulled {
                let mut refs = Vec::new();
                collect_refs(conjunct, &mut refs);
                for (b, c) in refs {
                    if inside.contains(&b) && !exposed.contains_key(&(b, c)) {
                        let existing = exprs.iter().position(|e| e.expr == column(b, c));
                        let position = existing.unwrap_or_else(|| {
                            exprs.push(NamedExpr {
                                expr: column(b, c),
                                name: None,
                            });
                            exprs.len() - 1
                        });
                        exposed.insert((b, c), col_id(position));
                    }
                }
            }
            for conjunct in &mut pulled {
                super::util::rewrite_expr(conjunct, &|b, c| {
                    exposed.get(&(b, c)).map(|&pos| column(binding_id, pos))
                });
            }
            Some((
                Rel::Project {
                    input: Box::new(input),
                    exprs,
                    binding,
                },
                pulled,
            ))
        }
        Rel::Sort { input, keys } => {
            if keys.iter().any(|k| touches(&k.expr, outer)) {
                return None;
            }
            let (input, pulled) = pull(*input, outer, counts_matter)?;
            Some((
                Rel::Sort {
                    input: Box::new(input),
                    keys,
                },
                pulled,
            ))
        }
        Rel::Distinct { input } if !counts_matter => {
            let (input, pulled) = pull(*input, outer, counts_matter)?;
            Some((
                Rel::Distinct {
                    input: Box::new(input),
                },
                pulled,
            ))
        }
        _ => None,
    }
}

/// Trace `(binding, column)` of `rel`'s output back to a column of the
/// single table `rel` reads, through filters, sorts and column-passing
/// projections. `None` if `rel` is not such a chain or the column is computed.
fn base_column(
    rel: &Rel,
    binding: BindingId,
    column: ColumnId,
) -> Option<(crate::names::TableId, ColumnId)> {
    match rel {
        Rel::Get {
            table: crate::names::TableRef::Id(t),
            binding: Some(b),
            ..
        } if *b == binding => Some((*t, column)),
        Rel::Filter { input, .. } | Rel::Sort { input, .. } => base_column(input, binding, column),
        Rel::Project {
            input,
            exprs,
            binding: Some(p),
        } if *p == binding => match &exprs.get(usize::try_from(column.get()).ok()?)?.expr {
            Expr::Column(crate::names::ColumnRef::Bound { binding, column }) => {
                base_column(input, *binding, *column)
            }
            _ => None,
        },
        _ => None,
    }
}

/// Collect every bound column reference in `expr`.
fn collect_refs(expr: &Expr, out: &mut Vec<(BindingId, ColumnId)>) {
    if let Expr::Column(crate::names::ColumnRef::Bound { binding, column }) = expr {
        out.push((*binding, *column));
    }
    for child in expr.children() {
        collect_refs(child, out);
    }
}

/// Split pulled conjuncts into `(inner, outer)` expression pairs of equalities.
///
/// Each conjunct must be `a = b` with one side referencing only `inner`
/// bindings (at least one) and the other none of them, and the inner side
/// comparable. Returns `None` otherwise.
fn equalities(
    pulled: Vec<Expr>,
    inner: &HashSet<BindingId>,
    types: &Types,
    catalog: &dyn Catalog,
) -> Option<Vec<(Expr, Expr)>> {
    pulled
        .into_iter()
        .map(|conjunct| {
            let Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
            } = conjunct
            else {
                return None;
            };
            let side = |e: &Expr| -> Option<bool> {
                let refs = bindings_of(e);
                let in_count = refs.iter().filter(|b| inner.contains(b)).count();
                match (in_count, refs.len() - in_count) {
                    (n, 0) if n > 0 => Some(true),
                    (0, _) => Some(false),
                    _ => None,
                }
            };
            let (inner_side, outer_side) = match (side(&left)?, side(&right)?) {
                (true, false) => (*left, *right),
                (false, true) => (*right, *left),
                _ => return None,
            };
            // Columns exposed by pulling are not in `types` yet; both sides of `=` share a type.
            let ty = data_type(&inner_side, types, catalog)
                .or_else(|| data_type(&outer_side, types, catalog))?;
            typing::is_comparable(&ty).then_some((inner_side, outer_side))
        })
        .collect()
}

/// Return the join condition matching group keys `binding.0..n` to outer expressions.
fn key_condition(binding: BindingId, outer_exprs: Vec<Expr>, offset: usize) -> Expr {
    conjoin(
        outer_exprs
            .into_iter()
            .enumerate()
            .map(|(i, e)| Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(column(binding, col_id(offset + i))),
                right: Box::new(e),
            })
            .collect(),
    )
}

impl Decorrelator<'_> {
    /// Decorrelate `rel` and everything below it.
    pub(crate) fn run(&mut self, rel: &mut Rel) {
        for input in rel.inputs_mut() {
            self.run(input);
        }
        if let Rel::Apply { .. } = rel {
            if self.frozen {
                return;
            }
            let Rel::Apply { kind, outer, inner } = take(rel) else {
                unreachable!()
            };
            *rel = match self.apply(kind, *outer, *inner) {
                Ok(rewritten) => {
                    self.changed = true;
                    self.frozen = !self.remaps.is_empty();
                    rewritten
                }
                Err((outer, inner)) => Rel::Apply {
                    kind,
                    outer: Box::new(outer),
                    inner: Box::new(inner),
                },
            };
        }
    }

    /// Rewrite one apply as a join if a rule fits; otherwise give its inputs back.
    #[allow(clippy::result_large_err)]
    fn apply(&mut self, kind: ApplyKind, outer: Rel, inner: Rel) -> Result<Rel, (Rel, Rel)> {
        let outer_bindings = produced(&outer);
        let join = |kind: JoinKind, outer: Rel, inner: Rel, on: Expr| Rel::Join {
            kind,
            left: Box::new(outer),
            right: Box::new(inner),
            on,
        };
        if free(&inner).is_disjoint(&outer_bindings) {
            let kind = match kind {
                ApplyKind::Inner => JoinKind::Inner,
                ApplyKind::Left => JoinKind::Left,
                ApplyKind::Semi => JoinKind::Semi,
                ApplyKind::Anti => JoinKind::Anti,
                ApplyKind::Single if super::util::at_most_one_row(&inner) => JoinKind::Left,
                ApplyKind::Single => return Err((outer, inner)),
            };
            return Ok(join(kind, outer, inner, Expr::Literal(Value::Bool(true))));
        }
        let join_kind = match kind {
            ApplyKind::Semi => Some(JoinKind::Semi),
            ApplyKind::Anti => Some(JoinKind::Anti),
            ApplyKind::Inner => Some(JoinKind::Inner),
            ApplyKind::Left => Some(JoinKind::Left),
            ApplyKind::Single => None,
        };
        if let Some(join_kind) = join_kind {
            let counts_matter = matches!(kind, ApplyKind::Inner | ApplyKind::Left);
            if let Some((inner, pulled)) = pull(inner.clone(), &outer_bindings, counts_matter) {
                return Ok(join(join_kind, outer, inner, conjoin(pulled)));
            }
        }
        if kind == ApplyKind::Single
            && let Some(rel) = self.single_by_key(&outer, &inner, &outer_bindings)
        {
            return Ok(join(JoinKind::Left, outer, rel.0, rel.1));
        }
        if matches!(kind, ApplyKind::Inner | ApplyKind::Left | ApplyKind::Single) {
            match inner {
                Rel::Aggregate { ref keys, .. } if keys.is_empty() => {
                    return self.aggregate(outer, inner, &outer_bindings);
                }
                Rel::Nest { ref keys, .. } if keys.is_empty() => {
                    return self.nest(outer, inner, &outer_bindings);
                }
                _ => {}
            }
        }
        if kind == ApplyKind::Left {
            return self.exists_marker(outer, inner, &outer_bindings);
        }
        Err((outer, inner))
    }

    /// Pull equality correlation out of `input`; return it uncorrelated with `(inner, outer)` key pairs.
    fn keyed(
        &self,
        input: &Rel,
        outer: &HashSet<BindingId>,
        counts_matter: bool,
    ) -> Option<(Rel, Vec<(Expr, Expr)>)> {
        let (input, pulled) = pull(input.clone(), outer, counts_matter)?;
        if pulled.is_empty() {
            return None;
        }
        let inside = produced(&input);
        let pairs = equalities(pulled, &inside, &self.types, self.catalog)?;
        Some((input, pairs))
    }

    /// Decorrelate a single apply whose correlation fixes the inner table's primary key.
    ///
    /// Equalities on every primary key column of the one table the inner
    /// side reads mean at most one inner row per outer row, so the single
    /// apply is a left join. Returns the uncorrelated inner side and the
    /// join condition.
    fn single_by_key(
        &self,
        _outer: &Rel,
        inner: &Rel,
        outer: &HashSet<BindingId>,
    ) -> Option<(Rel, Expr)> {
        let (input, pulled) = pull(inner.clone(), outer, true)?;
        if pulled.is_empty() {
            return None;
        }
        let pairs = equalities(pulled.clone(), &produced(&input), &self.types, self.catalog)?;
        let mut table = None;
        let mut fixed = HashSet::new();
        for (inner_side, _) in &pairs {
            let Expr::Column(crate::names::ColumnRef::Bound { binding, column }) = inner_side
            else {
                continue;
            };
            let (t, c) = base_column(&input, *binding, *column)?;
            if table.is_some_and(|known| known != t) {
                return None;
            }
            table = Some(t);
            fixed.insert(c);
        }
        let def = self.catalog.table_by_id(table?)?;
        let unique =
            !def.primary_key.is_empty() && def.primary_key.iter().all(|c| fixed.contains(c));
        unique.then(|| (input, conjoin(pulled)))
    }

    /// Rewrite an apply over a keyless aggregate as a left join to a grouped aggregate.
    #[allow(clippy::result_large_err)]
    fn aggregate(
        &mut self,
        outer: Rel,
        inner: Rel,
        outer_bindings: &HashSet<BindingId>,
    ) -> Result<Rel, (Rel, Rel)> {
        let Rel::Aggregate {
            input,
            aggs,
            binding: Some(old),
            ..
        } = &inner
        else {
            return Err((outer, inner));
        };
        let agg_exprs_correlated = aggs.iter().any(|a| {
            a.agg
                .args
                .iter()
                .chain(a.agg.filter.iter())
                .any(|e| touches(e, outer_bindings))
        });
        if agg_exprs_correlated {
            return Err((outer, inner));
        }
        let Some((input, pairs)) = self.keyed(input, outer_bindings, true) else {
            return Err((outer, inner));
        };
        let (inner_keys, outer_exprs): (Vec<Expr>, Vec<Expr>) = pairs.into_iter().unzip();
        let n = inner_keys.len();
        let new = self.ids.fresh();
        let replacements = aggs
            .iter()
            .enumerate()
            .map(|(j, a)| {
                let reference = column(new, col_id(n + j));
                let is_count = matches!(&a.agg.func, FunctionRef::Id(id)
                    if self.catalog.function_by_id(*id).is_some_and(|f| f.name.to_string() == "count"));
                if is_count {
                    Expr::Coalesce(vec![reference, Expr::Literal(Value::Int8(0))])
                } else {
                    reference
                }
            })
            .collect();
        self.remaps.insert(*old, replacements);
        let grouped = Rel::Aggregate {
            input: Box::new(input),
            keys: inner_keys.into_iter().map(NamedExpr::from).collect(),
            aggs: aggs.clone(),
            binding: Some(new),
        };
        Ok(Rel::Join {
            kind: JoinKind::Left,
            left: Box::new(outer),
            right: Box::new(grouped),
            on: key_condition(new, outer_exprs, 0),
        })
    }

    /// Rewrite an apply over a keyless nest as a left join to a keyed nest.
    #[allow(clippy::result_large_err)]
    fn nest(
        &mut self,
        outer: Rel,
        inner: Rel,
        outer_bindings: &HashSet<BindingId>,
    ) -> Result<Rel, (Rel, Rel)> {
        let Rel::Nest {
            input,
            fields,
            name,
            binding: Some(old),
            ..
        } = &inner
        else {
            return Err((outer, inner));
        };
        if fields.iter().any(|f| touches(&f.expr, outer_bindings)) {
            return Err((outer, inner));
        }
        let Some(nested_type) = self
            .types
            .get(&(*old, ColumnId::new(0)))
            .map(|t| t.data.clone())
        else {
            return Err((outer, inner));
        };
        let Some((input, pairs)) = self.keyed(input, outer_bindings, true) else {
            return Err((outer, inner));
        };
        let (inner_keys, outer_exprs): (Vec<Expr>, Vec<Expr>) = pairs.into_iter().unzip();
        let n = inner_keys.len();
        let new = self.ids.fresh();
        let empty = Expr::Cast {
            expr: Box::new(Expr::Array(Vec::new())),
            to: nested_type,
        };
        self.remaps.insert(
            *old,
            vec![Expr::Coalesce(vec![column(new, col_id(n)), empty])],
        );
        let keyed = Rel::Nest {
            input: Box::new(input),
            keys: inner_keys.into_iter().map(NamedExpr::from).collect(),
            fields: fields.clone(),
            name: name.clone(),
            binding: Some(new),
        };
        Ok(Rel::Join {
            kind: JoinKind::Left,
            left: Box::new(outer),
            right: Box::new(keyed),
            on: key_condition(new, outer_exprs, 0),
        })
    }

    /// Rewrite a left apply of one `true` marker row (lowered `EXISTS`) as a left join.
    #[allow(clippy::result_large_err)]
    fn exists_marker(
        &mut self,
        outer: Rel,
        inner: Rel,
        outer_bindings: &HashSet<BindingId>,
    ) -> Result<Rel, (Rel, Rel)> {
        let Rel::Project {
            input,
            exprs,
            binding,
        } = &inner
        else {
            return Err((outer, inner));
        };
        let marker = matches!(exprs.as_slice(), [e] if e.expr == Expr::Literal(Value::Bool(true)));
        let Rel::Limit {
            input: limited,
            limit: Some(Expr::Literal(Value::Int8(1))),
            offset: None,
        } = input.as_ref()
        else {
            return Err((outer, inner));
        };
        if !marker {
            return Err((outer, inner));
        }
        let Some(binding) = *binding else {
            return Err((outer, inner));
        };
        let Some((limited, pairs)) = self.keyed(limited, outer_bindings, false) else {
            return Err((outer, inner));
        };
        let (inner_keys, outer_exprs): (Vec<Expr>, Vec<Expr>) = pairs.into_iter().unzip();
        let mut new_exprs = vec![exprs[0].clone()];
        new_exprs.extend(inner_keys.into_iter().map(NamedExpr::from));
        // The binding stays: the marker is still column 0, keys follow it.
        let distinct = Rel::Distinct {
            input: Box::new(Rel::Project {
                input: Box::new(limited),
                exprs: new_exprs,
                binding: Some(binding),
            }),
        };
        Ok(Rel::Join {
            kind: JoinKind::Left,
            left: Box::new(outer),
            right: Box::new(distinct),
            on: key_condition(binding, outer_exprs, 1),
        })
    }
}

/// Decorrelate `plan` to a fixpoint, rewriting references to removed bindings between passes.
pub(crate) fn decorrelate(plan: &mut crate::rel::Plan, catalog: &dyn Catalog) {
    for _ in 0..64 {
        let mut decorrelator = Decorrelator {
            catalog,
            types: crate::verify::column_types(plan, catalog),
            ids: Bindings(super::util::max_binding(&plan.root)),
            remaps: HashMap::new(),
            frozen: false,
            changed: false,
        };
        decorrelator.run(&mut plan.root);
        if !decorrelator.remaps.is_empty() {
            let remaps = decorrelator.remaps;
            rewrite_rel(&mut plan.root, &|b, c| {
                remaps
                    .get(&b)
                    .and_then(|columns| columns.get(usize::try_from(c.get()).ok()?))
                    .cloned()
            });
        }
        if !decorrelator.changed {
            return;
        }
    }
}
