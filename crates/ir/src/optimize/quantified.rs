//! Decorrelation of quantified comparisons: `IN`, `NOT IN`, `op ANY` and
//! `op ALL` (milestone F3, backlog `not-in-decorrelation`).
//!
//! The resolver lowers `x op ANY (s)` (and `IN`, and through negation
//! `NOT IN` and `ALL`) to a single apply over a keyless aggregate of `s`:
//! `bool_or(x op c)`, `count(*)` and `count(x op c)`, from which a `CASE`
//! derives the three-valued result. Its arguments read the outer row, so
//! the scalar-aggregate rule can't decorrelate it, and it ran as a nested
//! loop. This rule computes the same three values set-oriented, from
//! aggregates of `s` that don't read the outer row:
//!
//! - `rows` is `count(*)` of `s`;
//! - `known`, the comparisons that are not NULL, is 0 when `x` is NULL and
//!   otherwise `nn`, the number of non-NULL `c`;
//! - `any_true` is NULL when `x` is NULL or `nn` is 0 (no comparison is
//!   known), and otherwise: for `=`, whether `x` is among the non-NULL `c`
//!   (a left join to their distinct values: a null-aware anti join once
//!   `NOT` and the filter apply); for `<` and `<=`, `x op max(c)`; for `>`
//!   and `>=`, `x op min(c)`; for `<>`, whether `x` differs from `min(c)` or
//!   `max(c)`.
//!
//! The aggregates join the outer side on the subquery's equality
//! correlation, if any (grouped by its inner expressions, as the
//! scalar-aggregate rule does), or on `true`. `x` and `c` must be leakproof,
//! so evaluating them for rows the nested loop would not have compared
//! raises nothing, and the subquery must not be volatile, since the rule
//! reads it twice.

use std::collections::{HashMap, HashSet};

use super::decorrelate::{Decorrelator, key_condition, touches};
use super::util::{
    Bindings, bindings_of, col_id, column, conjoin, data_type, free, leakproof, produced,
    rewrite_expr, rewrite_rel, volatile,
};
use crate::expr::{BinaryOp, CaseBranch, Expr, NamedExpr, UnaryOp};
use crate::names::{BindingId, FunctionRef};
use crate::rel::{AggCall, JoinKind, NamedAgg, Rel};
use crate::types::{DataType, ScalarType};
use crate::value::Value;

/// What one aggregate of the lowered subquery computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// `bool_or(x op c)`: whether any comparison is TRUE.
    AnyTrue,
    /// `count(*)`: the subquery's rows.
    Rows,
    /// `count(x op c)`: the comparisons that are not NULL.
    Known,
}

/// A lowered quantified comparison, taken apart.
struct Quantified {
    /// The subquery, without its equality correlation.
    input: Rel,
    /// The correlation: (inner expression, outer expression) pairs.
    pairs: Vec<(Expr, Expr)>,
    /// The outer operand.
    x: Expr,
    /// The comparison, with `x` on the left.
    op: BinaryOp,
    /// The subquery operand.
    c: Expr,
    /// The type of `c`.
    c_type: DataType,
    /// What each aggregate computes, in order.
    roles: Vec<Role>,
    /// The aggregate's binding, whose columns the outer side reads.
    binding: BindingId,
}

/// Return the comparison with its operands swapped.
fn flip(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::Le => BinaryOp::Ge,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::Ge => BinaryOp::Le,
        other => other,
    }
}

/// Return `x op c` from a comparison, with `x` the side reading only `outer`.
fn split_comparison(
    comparison: &Expr,
    outer: &HashSet<BindingId>,
) -> Option<(Expr, BinaryOp, Expr)> {
    let Expr::Binary { op, left, right } = comparison else {
        return None;
    };
    if !matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    ) {
        return None;
    }
    let outer_only = |e: &Expr| {
        let read = bindings_of(e);
        !read.is_empty() && read.is_subset(outer)
    };
    if outer_only(left) && !touches(right, outer) {
        Some(((**left).clone(), *op, (**right).clone()))
    } else if outer_only(right) && !touches(left, outer) {
        Some(((**right).clone(), flip(*op), (**left).clone()))
    } else {
        None
    }
}

/// Return whether any expression of `rel` or below calls a volatile function.
fn volatile_rel(rel: &Rel, catalog: &dyn crate::catalog::Catalog) -> bool {
    rel.exprs().into_iter().any(|e| volatile(e, catalog))
        || rel.inputs().into_iter().any(|i| volatile_rel(i, catalog))
}

/// Give every binding `rel` introduces a fresh one from `ids`, renaming references to match.
///
/// A plan may introduce each binding once, so a subquery read twice needs a
/// renamed copy; returns the renaming, for expressions over its output.
fn rebind(rel: &mut Rel, ids: &mut Bindings) -> HashMap<BindingId, BindingId> {
    /// Replace the bindings of `rel` and below, recording each renaming.
    fn fresh(rel: &mut Rel, ids: &mut Bindings, renamed: &mut HashMap<BindingId, BindingId>) {
        match rel {
            Rel::Get { binding, .. }
            | Rel::Project { binding, .. }
            | Rel::Aggregate { binding, .. }
            | Rel::SetOp { binding, .. }
            | Rel::Values { binding, .. }
            | Rel::Nest { binding, .. }
            | Rel::Unnest { binding, .. } => {
                if let Some(old) = *binding {
                    let new = ids.fresh();
                    renamed.insert(old, new);
                    *binding = Some(new);
                }
            }
            _ => {}
        }
        for input in rel.inputs_mut() {
            fresh(input, ids, renamed);
        }
    }
    let mut renamed = HashMap::new();
    fresh(rel, ids, &mut renamed);
    rewrite_rel(rel, &|b, c| renamed.get(&b).map(|&new| column(new, c)));
    renamed
}

/// Rename references in `expr` by `renamed`.
fn renamed(mut expr: Expr, renamed: &HashMap<BindingId, BindingId>) -> Expr {
    rewrite_expr(&mut expr, &|b, c| {
        renamed.get(&b).map(|&new| column(new, c))
    });
    expr
}

/// Construct `NULL` of type `bool`.
fn null_bool() -> Expr {
    Expr::Cast {
        expr: Box::new(Expr::Literal(Value::Null)),
        to: DataType::Scalar(ScalarType::Bool),
    }
}

/// Construct `expr IS NULL` (or `IS NOT NULL`).
fn is_null(expr: Expr, negated: bool) -> Expr {
    Expr::Unary {
        op: if negated {
            UnaryOp::IsNotNull
        } else {
            UnaryOp::IsNull
        },
        expr: Box::new(expr),
    }
}

/// Construct `left op right`.
fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

/// Construct `coalesce(expr, 0)` for a count.
fn or_zero(expr: Expr) -> Expr {
    Expr::Coalesce(vec![expr, Expr::Literal(Value::Int8(0))])
}

impl Decorrelator<'_> {
    /// Look up the built-in aggregate `name` over `args`.
    fn builtin_aggregate(&self, name: &str, args: &[DataType]) -> Option<FunctionRef> {
        self.catalog
            .function_by_name(&name.into(), args)
            .map(|f| FunctionRef::Id(f.id))
    }

    /// Return the name of the function `func` refers to.
    fn function_name(&self, func: &FunctionRef) -> Option<String> {
        match func {
            FunctionRef::Id(id) => self.catalog.function_by_id(*id).map(|f| f.name.to_string()),
            FunctionRef::Named(_) => None,
        }
    }

    /// Take apart a keyless aggregate that lowers a quantified comparison, if `inner` is one.
    fn quantified_parts(&self, inner: &Rel, outer: &HashSet<BindingId>) -> Option<Quantified> {
        let Rel::Aggregate {
            input,
            keys,
            aggs,
            binding: Some(binding),
        } = inner
        else {
            return None;
        };
        if !keys.is_empty() || aggs.is_empty() {
            return None;
        }
        let mut comparison: Option<&Expr> = None;
        let mut roles = Vec::with_capacity(aggs.len());
        for NamedAgg { agg, .. } in aggs {
            if agg.distinct || agg.filter.is_some() {
                return None;
            }
            let name = self.function_name(&agg.func)?;
            let role = match (name.as_str(), agg.args.as_slice()) {
                ("bool_or", [arg]) => {
                    if comparison.is_some_and(|c| c != arg) {
                        return None;
                    }
                    comparison = Some(arg);
                    Role::AnyTrue
                }
                ("count", []) => Role::Rows,
                ("count", [arg]) => {
                    if comparison.is_some_and(|c| c != arg) {
                        return None;
                    }
                    comparison = Some(arg);
                    Role::Known
                }
                _ => return None,
            };
            roles.push(role);
        }
        let (x, op, c) = split_comparison(comparison?, outer)?;
        if !leakproof(&x, &self.types, self.catalog)
            || !leakproof(&c, &self.types, self.catalog)
            || volatile_rel(input, self.catalog)
        {
            return None;
        }
        let c_type = data_type(&c, &self.types, self.catalog)?;
        let (input, pairs) = if free(input).is_disjoint(outer) {
            ((**input).clone(), Vec::new())
        } else {
            self.keyed(input, outer, true)?
        };
        // `c` must read only what the subquery produces, now that its correlation is pulled out.
        if !bindings_of(&c).is_subset(&produced(&input)) {
            return None;
        }
        Some(Quantified {
            input,
            pairs,
            x,
            op,
            c,
            c_type,
            roles,
            binding: *binding,
        })
    }

    /// Rewrite an apply over a lowered quantified comparison as left joins to
    /// uncorrelated aggregates of the subquery; give the inputs back if it isn't one.
    #[allow(clippy::result_large_err)]
    pub(super) fn quantified(
        &mut self,
        outer: Rel,
        inner: Rel,
        outer_bindings: &HashSet<BindingId>,
    ) -> Result<Rel, (Rel, Rel)> {
        let Some(q) = self.quantified_parts(&inner, outer_bindings) else {
            return Err((outer, inner));
        };
        let Some(rewritten) = self.rewrite_quantified(outer.clone(), q) else {
            return Err((outer, inner));
        };
        Ok(rewritten)
    }

    /// Build the joins and the replacement of each aggregate column.
    fn rewrite_quantified(&mut self, outer: Rel, q: Quantified) -> Option<Rel> {
        let Quantified {
            input,
            pairs,
            x,
            op,
            c,
            c_type,
            roles,
            binding,
        } = q;
        let (inner_keys, outer_exprs): (Vec<Expr>, Vec<Expr>) = pairs.into_iter().unzip();
        let n = inner_keys.len();
        let agg = |func, args: Vec<Expr>, name: &str| NamedAgg {
            agg: AggCall {
                func,
                args,
                distinct: false,
                filter: None,
            },
            name: Some(name.into()),
        };
        let mut aggs = vec![
            agg(self.builtin_aggregate("count", &[])?, Vec::new(), "rows"),
            agg(
                self.builtin_aggregate("count", std::slice::from_ref(&c_type))?,
                vec![c.clone()],
                "nn",
            ),
        ];
        let extremes: &[&str] = match op {
            BinaryOp::Lt | BinaryOp::Le => &["max"],
            BinaryOp::Gt | BinaryOp::Ge => &["min"],
            BinaryOp::Ne => &["min", "max"],
            _ => &[],
        };
        for name in extremes {
            aggs.push(agg(
                self.builtin_aggregate(name, std::slice::from_ref(&c_type))?,
                vec![c.clone()],
                name,
            ));
        }
        let stats = self.ids.fresh();
        let stat = |i: usize| column(stats, col_id(n + i));
        let mut joined = Rel::Join {
            kind: JoinKind::Left,
            left: Box::new(outer),
            right: Box::new(Rel::Aggregate {
                input: Box::new(input.clone()),
                keys: inner_keys.iter().cloned().map(NamedExpr::from).collect(),
                aggs,
                binding: Some(stats),
            }),
            on: key_condition(stats, outer_exprs.clone(), 0),
        };
        let rows = or_zero(stat(0));
        let nn = or_zero(stat(1));
        let unknown = Expr::Or(vec![
            is_null(x.clone(), false),
            binary(BinaryOp::Eq, nn.clone(), Expr::Literal(Value::Int8(0))),
        ]);
        let decided = |found: Expr| Expr::Case {
            operand: None,
            branches: vec![CaseBranch {
                when: unknown.clone(),
                then: null_bool(),
            }],
            otherwise: Some(Box::new(found)),
        };
        let any_true = match op {
            BinaryOp::Eq => {
                // The distinct non-NULL values of `c` (with the correlation keys), joined on
                // `x`, from a copy of the subquery under fresh bindings.
                let mut copy = input;
                let names = rebind(&mut copy, &mut self.ids);
                let c = renamed(c, &names);
                let values = self.ids.fresh();
                let mut exprs = vec![NamedExpr::new(c.clone(), "value")];
                exprs.extend(
                    inner_keys
                        .into_iter()
                        .map(|k| NamedExpr::from(renamed(k, &names))),
                );
                let distinct = Rel::Distinct {
                    input: Box::new(Rel::Project {
                        input: Box::new(Rel::Filter {
                            input: Box::new(copy),
                            predicate: is_null(c, true),
                        }),
                        exprs,
                        binding: Some(values),
                    }),
                };
                let found = column(values, col_id(0));
                joined = Rel::Join {
                    kind: JoinKind::Left,
                    left: Box::new(joined),
                    right: Box::new(distinct),
                    on: conjoin(vec![
                        binary(BinaryOp::Eq, found.clone(), x.clone()),
                        key_condition(values, outer_exprs, 1),
                    ]),
                };
                Expr::Case {
                    operand: None,
                    branches: vec![
                        CaseBranch {
                            when: is_null(found, true),
                            then: Expr::Literal(Value::Bool(true)),
                        },
                        CaseBranch {
                            when: unknown.clone(),
                            then: null_bool(),
                        },
                    ],
                    otherwise: Some(Box::new(Expr::Literal(Value::Bool(false)))),
                }
            }
            BinaryOp::Ne => decided(Expr::Or(vec![
                binary(BinaryOp::Ne, x.clone(), stat(2)),
                binary(BinaryOp::Ne, x.clone(), stat(3)),
            ])),
            op => decided(binary(op, x.clone(), stat(2))),
        };
        let known = Expr::Case {
            operand: None,
            branches: vec![CaseBranch {
                when: is_null(x, false),
                then: Expr::Literal(Value::Int8(0)),
            }],
            otherwise: Some(Box::new(nn)),
        };
        let replacements = roles
            .iter()
            .map(|role| match role {
                Role::AnyTrue => any_true.clone(),
                Role::Rows => rows.clone(),
                Role::Known => known.clone(),
            })
            .collect();
        self.remaps.insert(binding, replacements);
        Some(joined)
    }
}
