//! Two-valued logic over optional values (ADR-0034).
//!
//! Resolved IR is two-valued: predicates are plain `bool`, `and`, `or` and
//! `not` take plain `bool`s, and a strict operator or function is applied
//! to an optional value only inside [`Expr::Lift`]. IR text, and the rules
//! that rewrite plans, may still write SQL's shorthand: `(= u.age 30)` on
//! an optional `age`, or `and` over optional booleans, with SQL's
//! three-valued meaning. [`normalize`] turns that shorthand into the
//! explicit two-valued form:
//!
//! - a strict operator or function with an optional operand is wrapped in
//!   `lift`, which propagates absence as SQL does;
//! - `and`, `or` and `not` over optional booleans become the functions
//!   `and3`, `or3` and `not3`;
//! - a predicate (a filter, join condition, policy residual, write check,
//!   aggregate filter or `CASE` condition) that may be absent is made
//!   plain. Its `coalesce` with `false` is pushed through `and3`, `or3` and
//!   `not3`, so the result is `and` and `or` of atoms, each optional atom
//!   written `(coalesce (lift (cmp a b)) false)`. That is exactly what the
//!   predicate meant in SQL, where only TRUE keeps a row.
//!
//! The shorthand always means SQL's semantics, and a normalized plan holds
//! no shorthand, so normalizing twice changes nothing. Total equality on
//! optional values is written `is-not-distinct-from`, never `=`.

use std::collections::{HashMap, HashSet};

use crate::catalog::Catalog;
use crate::expr::{BinaryOp, CaseBranch, Expr, UnaryOp};
use crate::names::{BindingId, ColumnId, ColumnRef, FunctionId, FunctionRef, QualifiedName};
use crate::rel::{ApplyKind, JoinKind, Plan, Rel};
use crate::types::{DataType, ScalarType, Type};
use crate::value::Value;

/// Return the comparison a predicate atom tests, as `(op, left, right)`.
///
/// Matches `(op a b)` on plain values and its normalized form on optional
/// ones, `(coalesce (lift (op a b)) false)`. Both are true exactly when
/// both operands are present and the comparison holds, so index selection,
/// pushdown and join planning can treat them alike.
///
/// # Examples
///
/// ```
/// use interlaken_ir::BinaryOp;
/// use interlaken_ir::logic::comparison;
/// use interlaken_ir::text::parse_expr;
///
/// let atom = parse_expr("(coalesce (lift (< #1.2 5)) false)")?;
/// let (op, left, _) = comparison(&atom).unwrap();
/// assert_eq!((op, left), (BinaryOp::Lt, &parse_expr("#1.2")?));
/// assert!(comparison(&parse_expr("(coalesce (lift (< #1.2 5)) true)")?).is_none());
/// # Ok::<(), interlaken_ir::text::ParseError>(())
/// ```
#[must_use]
pub fn comparison(expr: &Expr) -> Option<(BinaryOp, &Expr, &Expr)> {
    let inner = match expr {
        Expr::Coalesce(items) => match items.as_slice() {
            [Expr::Lift(inner), Expr::Literal(Value::Bool(false))] => inner.as_ref(),
            _ => return None,
        },
        other => other,
    };
    match inner {
        Expr::Binary { op, left, right } if op.is_comparison() => Some((*op, left, right)),
        _ => None,
    }
}

/// Return `expr` without an enclosing `lift`.
#[must_use]
pub fn unlifted(expr: &Expr) -> &Expr {
    match expr {
        Expr::Lift(inner) => inner,
        other => other,
    }
}

/// Rewrite `plan`'s shorthand into explicit two-valued forms (see the module documentation).
///
/// Column types come from [`crate::verify::column_types`], which gives
/// each column's type where it is introduced; above an outer join or a
/// left or single apply, the padded side's columns are optional whatever
/// their type, which the pass tracks. A column whose type can't be found
/// is treated as optional, which only adds a `lift` or `coalesce` that
/// changes nothing.
///
/// # Examples
///
/// ```
/// use interlaken_ir::logic::normalize;
/// use interlaken_ir::text::{parse_catalog, parse_plan, print_plan};
///
/// let catalog = parse_catalog(
///     "(catalog (builtins) (table t :id 100 (column x int8) (column y (option int8))))",
/// )?;
/// let mut plan = parse_plan(
///     "(plan (filter (get (tid 100) :bind 1) (and (> #1.0 1) (= #1.1 2))) :schema-version 1)",
/// )?;
/// normalize(&mut plan, &catalog);
/// assert_eq!(
///     print_plan(&plan),
///     "(plan :schema-version 1\n  (filter\n    (get (tid 100) :bind 1)\n    \
///      (and (> #1.0 1) (coalesce (lift (= #1.1 2)) false))))"
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn normalize(plan: &mut Plan, catalog: &dyn Catalog) {
    let types = crate::verify::column_types(plan, catalog);
    let normalizer = Normalizer::new(types, catalog, false);
    normalizer.rel(&mut plan.root, &HashSet::new());
}

/// Normalize `plan` as [`normalize`] does, and also apply the rewrites
/// two-valued logic makes valid (ADR-0034 phase 4). The optimizer uses
/// this; the resolver doesn't, so resolved plans show what was written.
///
/// - `(is-null c)` of a column, parameter or literal that can't be absent
///   is `false`, and `(is-not-null c)` is `true`.
/// - `(not (op a b))` of a comparison of values that can't be absent is
///   the negated comparison: `(not (< a b))` is `(>= a b)`. Comparisons
///   are total orders on plain values, `nan` included.
/// - `(not (coalesce (lift (op a b)) x))` is `(coalesce (lift (negated op
///   a b)) (not x))`, so `not` over SQL-style comparisons becomes a
///   comparison that index selection and pushdown recognise. `not` moves
///   through `and` and `or` by De Morgan's laws to reach them.
/// - Constant `and`, `or` and `not` fold.
pub(crate) fn normalize_folding(plan: &mut Plan, catalog: &dyn Catalog) {
    let types = crate::verify::column_types(plan, catalog);
    let normalizer = Normalizer::new(types, catalog, true);
    normalizer.rel(&mut plan.root, &HashSet::new());
}

/// The bindings whose columns are optional in a scope because an outer
/// join or apply padded them.
type Padded = HashSet<BindingId>;

/// The state of one normalization.
struct Normalizer<'a> {
    /// Column types.
    types: HashMap<(BindingId, ColumnId), Type>,
    /// The catalog, for function properties.
    catalog: &'a dyn Catalog,
    /// The IDs of `and3`, `or3` and `not3`, if the catalog has them.
    and3: Option<FunctionId>,
    /// See `and3`.
    or3: Option<FunctionId>,
    /// See `and3`.
    not3: Option<FunctionId>,
    /// Whether to apply the rewrites of [`normalize_folding`].
    fold: bool,
}

/// Return the literal `value`.
fn boolean(value: bool) -> Expr {
    Expr::Literal(Value::Bool(value))
}

/// Return `expr`, leaving an empty conjunction in its place.
fn take(expr: &mut Expr) -> Expr {
    std::mem::replace(expr, Expr::And(Vec::new()))
}

impl<'a> Normalizer<'a> {
    /// Start a normalization with `types` against `catalog`.
    fn new(
        types: HashMap<(BindingId, ColumnId), Type>,
        catalog: &'a dyn Catalog,
        fold: bool,
    ) -> Self {
        let bool = DataType::Scalar(ScalarType::Bool);
        let id = |name: &str, arity: usize| {
            catalog
                .function_by_name(&QualifiedName::from(name), &vec![bool.clone(); arity])
                .map(|f| f.id)
        };
        Self {
            and3: id("and3", 2),
            or3: id("or3", 2),
            not3: id("not3", 1),
            types,
            catalog,
            fold,
        }
    }

    /// Normalize every expression of `rel` and below, with `outer` the
    /// padded bindings of enclosing apply outer sides; return the padded
    /// bindings of `rel`'s output.
    fn rel(&self, rel: &mut Rel, outer: &Padded) -> Padded {
        let union = |a: &Padded, b: &Padded| -> Padded { a.union(b).copied().collect() };
        let (own, out): (Padded, Padded) = match &mut *rel {
            Rel::Join {
                kind, left, right, ..
            } => {
                let l = self.rel(left, outer);
                let r = self.rel(right, outer);
                let both = union(&l, &r);
                let out = match kind {
                    JoinKind::Inner => both.clone(),
                    JoinKind::Left => union(&both, &crate::optimize::produced(right)),
                    JoinKind::Right => union(&both, &crate::optimize::produced(left)),
                    JoinKind::Full => union(
                        &union(&both, &crate::optimize::produced(left)),
                        &crate::optimize::produced(right),
                    ),
                    JoinKind::Semi | JoinKind::Anti => l,
                };
                (both, out)
            }
            Rel::Apply {
                kind,
                outer: outer_rel,
                inner,
            } => {
                let o = self.rel(outer_rel, outer);
                let i = self.rel(inner, &union(outer, &o));
                let out = match kind {
                    ApplyKind::Inner => union(&o, &i),
                    ApplyKind::Left | ApplyKind::Single => {
                        union(&union(&o, &i), &crate::optimize::produced(inner))
                    }
                    ApplyKind::Semi | ApplyKind::Anti => o,
                };
                (Padded::new(), out)
            }
            other => {
                let mut seen = Padded::new();
                for input in other.inputs_mut() {
                    seen.extend(self.rel(input, outer));
                }
                // Operators that bind new columns output only those (their
                // types already reflect padding); the rest pass rows through.
                let out = match other {
                    Rel::Project { .. }
                    | Rel::Aggregate { .. }
                    | Rel::Values { .. }
                    | Rel::Nest { .. }
                    | Rel::SetOp { .. }
                    | Rel::Insert { .. }
                    | Rel::Update { .. }
                    | Rel::Delete { .. } => Padded::new(),
                    _ => seen.clone(),
                };
                (seen, out)
            }
        };
        let scope = union(&own, outer);
        let scope = &scope;
        match rel {
            Rel::Filter { predicate, .. }
            | Rel::Join { on: predicate, .. }
            | Rel::PolicyFilter {
                residual: predicate,
                ..
            }
            | Rel::WriteCheck {
                check: predicate, ..
            } => {
                *predicate = self.predicate(take(predicate), scope);
            }
            Rel::Aggregate { keys, aggs, .. } => {
                for key in keys {
                    key.expr = self.value(take(&mut key.expr), scope);
                }
                for agg in aggs {
                    for arg in &mut agg.agg.args {
                        *arg = self.value(take(arg), scope);
                    }
                    if let Some(filter) = &mut agg.agg.filter {
                        *filter = self.predicate(take(filter), scope);
                    }
                }
            }
            other => {
                for expr in other.exprs_mut() {
                    *expr = self.value(take(expr), scope);
                }
            }
        }
        out
    }

    /// Return `true` if `expr` may be absent, judged from the shorthand typing rules.
    fn optional(&self, expr: &Expr, scope: &Padded) -> bool {
        let optional = |e: &Expr| self.optional(e, scope);
        match expr {
            Expr::Column(ColumnRef::Bound { binding, column }) => {
                scope.contains(binding)
                    || self
                        .types
                        .get(&(*binding, *column))
                        .is_none_or(|t| t.optional)
            }
            Expr::Column(ColumnRef::Named { .. }) | Expr::Subquery { .. } | Expr::NullIf { .. } => {
                true
            }
            Expr::Literal(value) => matches!(value, Value::Null),
            Expr::Param { ty, .. } => ty.optional,
            Expr::Call { func, args } => {
                let FunctionRef::Id(id) = func else {
                    return true;
                };
                self.catalog.function_by_id(*id).is_none_or(|def| {
                    let flags: Vec<bool> = args.iter().map(&optional).collect();
                    crate::catalog::call_optional(&def, &flags)
                })
            }
            Expr::Lift(inner) => optional(inner),
            Expr::Unary { op, expr } => match op {
                UnaryOp::IsNull | UnaryOp::IsNotNull => false,
                UnaryOp::Not | UnaryOp::Neg => optional(expr),
            },
            Expr::Binary { op, left, right } => {
                !matches!(op, BinaryOp::IsDistinctFrom | BinaryOp::IsNotDistinctFrom)
                    && (optional(left) || optional(right))
            }
            Expr::And(items) | Expr::Or(items) => items.iter().any(optional),
            Expr::Case {
                branches,
                otherwise,
                ..
            } => {
                otherwise.as_ref().is_none_or(|e| optional(e))
                    || branches.iter().any(|b| optional(&b.then))
            }
            Expr::Coalesce(items) => items.iter().all(optional),
            Expr::Cast { expr, .. } => match expr.as_ref() {
                Expr::Literal(Value::Null) => true,
                Expr::Array(items) if items.is_empty() => false,
                inner => optional(inner),
            },
            Expr::Array(_) => false,
            Expr::Try { expr, default } => optional(expr) || optional(default),
        }
    }

    /// Return `true` if `func` is the function `id`.
    fn is(func: &FunctionRef, id: Option<FunctionId>) -> bool {
        matches!((func, id), (FunctionRef::Id(f), Some(id)) if *f == id)
    }

    /// Return a call of `id` with `args`, or `fallback` if the catalog lacks it.
    fn call(
        id: Option<FunctionId>,
        args: Vec<Expr>,
        fallback: impl FnOnce(Vec<Expr>) -> Expr,
    ) -> Expr {
        match id {
            Some(id) => Expr::Call {
                func: FunctionRef::Id(id),
                args,
            },
            None => fallback(args),
        }
    }

    /// Return `true` if the strict operator or call `expr` has an optional
    /// operand, so it must be lifted.
    fn needs_lift(&self, expr: &Expr, scope: &Padded) -> bool {
        let optional = |e: &Expr| self.optional(e, scope);
        match expr {
            Expr::Binary { op, left, right } => {
                !matches!(op, BinaryOp::IsDistinctFrom | BinaryOp::IsNotDistinctFrom)
                    && (optional(left) || optional(right))
            }
            Expr::Unary {
                op: UnaryOp::Neg,
                expr,
            } => optional(expr),
            Expr::Call {
                func: FunctionRef::Id(id),
                args,
            } => {
                self.catalog
                    .function_by_id(*id)
                    .is_some_and(|def| def.properties.strict)
                    && args.iter().any(optional)
            }
            _ => false,
        }
    }

    /// Normalize `expr` in a position that takes any value.
    fn value(&self, mut expr: Expr, scope: &Padded) -> Expr {
        match &mut expr {
            Expr::Case {
                operand: None,
                branches,
                otherwise,
            } => {
                for CaseBranch { when, then } in branches {
                    *when = self.predicate(take(when), scope);
                    *then = self.value(take(then), scope);
                }
                if let Some(e) = otherwise {
                    **e = self.value(take(e), scope);
                }
            }
            _ => {
                for child in expr.children_mut() {
                    *child = self.value(take(child), scope);
                }
            }
        }
        let normalized = match expr {
            Expr::Lift(inner) => match *inner {
                lifted @ Expr::Lift(_) => lifted,
                other if self.needs_lift(&other, scope) => Expr::Lift(Box::new(other)),
                other => other,
            },
            e @ (Expr::Binary { .. }
            | Expr::Unary {
                op: UnaryOp::Neg, ..
            }
            | Expr::Call { .. })
                if self.needs_lift(&e, scope) =>
            {
                Expr::Lift(Box::new(e))
            }
            Expr::Unary {
                op: UnaryOp::Not,
                expr: operand,
            } if self.optional(&operand, scope) => {
                Self::call(self.not3, vec![*operand], |mut args| Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(args.remove(0)),
                })
            }
            Expr::And(items) if items.iter().any(|e| self.optional(e, scope)) => {
                self.fold(items, self.and3, Expr::And)
            }
            Expr::Or(items) if items.iter().any(|e| self.optional(e, scope)) => {
                self.fold(items, self.or3, Expr::Or)
            }
            // Arguments after one that is never absent are never reached.
            Expr::Coalesce(mut items) => {
                if let Some(i) = items.iter().position(|e| !self.optional(e, scope)) {
                    items.truncate(i + 1);
                }
                if items.len() == 1 {
                    items.pop().expect("one argument")
                } else {
                    Expr::Coalesce(items)
                }
            }
            other => other,
        };
        self.simplify(normalized, scope)
    }

    /// Apply the rewrites of [`normalize_folding`] to the top of `expr`,
    /// whose operands are already normalized, if folding.
    fn simplify(&self, expr: Expr, scope: &Padded) -> Expr {
        if !self.fold {
            return expr;
        }
        // Only expressions that can't raise errors are folded away.
        let simple =
            |e: &Expr| matches!(e, Expr::Column(_) | Expr::Param { .. } | Expr::Literal(_));
        match expr {
            Expr::Unary {
                op: op @ (UnaryOp::IsNull | UnaryOp::IsNotNull),
                expr: operand,
            } if simple(&operand) && !self.optional(&operand, scope) => {
                boolean(op == UnaryOp::IsNotNull)
            }
            Expr::Unary {
                op: UnaryOp::Not,
                expr: operand,
            } => self.negate(*operand, scope),
            // A branch whose condition is false is never taken; one whose
            // condition is true is, if every branch before it was dropped.
            // With nothing left and no `else`, the result is NULL of the
            // case's type, which isn't written here, so that case stays.
            Expr::Case {
                operand: None,
                branches,
                otherwise,
            } if branches
                .iter()
                .any(|b| matches!(b.when, Expr::Literal(Value::Bool(_))))
                && (otherwise.is_some() || branches.iter().any(|b| b.when != boolean(false))) =>
            {
                let mut kept: Vec<CaseBranch> = Vec::new();
                for branch in branches {
                    match branch.when {
                        Expr::Literal(Value::Bool(false)) => {}
                        Expr::Literal(Value::Bool(true)) if kept.is_empty() => return branch.then,
                        _ => kept.push(branch),
                    }
                }
                match (kept.is_empty(), otherwise) {
                    (true, Some(otherwise)) => *otherwise,
                    (_, otherwise) => Expr::Case {
                        operand: None,
                        branches: kept,
                        otherwise,
                    },
                }
            }
            Expr::And(items) => {
                let items = match flatten_and(items) {
                    Expr::And(items) => items,
                    other => vec![other],
                };
                self.connective(items, false, Expr::And)
            }
            Expr::Or(items) => {
                let items = match flatten_or(items) {
                    Expr::Or(items) => items,
                    other => vec![other],
                };
                self.connective(items, true, Expr::Or)
            }
            other => other,
        }
    }

    /// Fold the constants of an `and` (`decisive` false) or `or` (`decisive`
    /// true) of `items`, as the optimizer's simplification does: the
    /// neutral constant goes, and the decisive one drops the operands after
    /// it, which never run, and those before it only if none can raise an
    /// error, which matters inside `try`.
    fn connective(&self, items: Vec<Expr>, decisive: bool, build: fn(Vec<Expr>) -> Expr) -> Expr {
        let mut kept = Vec::new();
        for item in items {
            match item {
                Expr::Literal(Value::Bool(b)) if b != decisive => {}
                Expr::Literal(Value::Bool(_)) => {
                    if kept
                        .iter()
                        .all(|e| crate::optimize::leakproof(e, &self.types, self.catalog))
                    {
                        kept.clear();
                    }
                    kept.push(boolean(decisive));
                    break;
                }
                other => kept.push(other),
            }
        }
        match kept.len() {
            0 => boolean(!decisive),
            1 => kept.pop().expect("one item"),
            _ => build(kept),
        }
    }

    /// Return `(not operand)`, rewritten where two-valued logic allows.
    fn negate(&self, operand: Expr, scope: &Padded) -> Expr {
        match operand {
            Expr::Literal(Value::Bool(b)) => boolean(!b),
            // De Morgan, which also keeps the operands' evaluation order.
            Expr::And(items) => self.simplify(
                Expr::Or(items.into_iter().map(|e| self.negate(e, scope)).collect()),
                scope,
            ),
            Expr::Or(items) => self.simplify(
                Expr::And(items.into_iter().map(|e| self.negate(e, scope)).collect()),
                scope,
            ),
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
            } => *expr,
            Expr::Binary { op, left, right }
                if op.is_comparison()
                    && !self.optional(&left, scope)
                    && !self.optional(&right, scope) =>
            {
                Expr::Binary {
                    op: crate::typing::negate_comparison(op),
                    left,
                    right,
                }
            }
            Expr::Coalesce(mut items)
                if items.len() == 2
                    && matches!(items[1], Expr::Literal(Value::Bool(_)))
                    && matches!(&items[0], Expr::Lift(inner)
                        if matches!(&**inner, Expr::Binary { op, .. } if op.is_comparison())) =>
            {
                let Some(Expr::Literal(Value::Bool(x))) = items.pop() else {
                    unreachable!("matched above")
                };
                let Some(Expr::Lift(inner)) = items.pop() else {
                    unreachable!("matched above")
                };
                let Expr::Binary { op, left, right } = *inner else {
                    unreachable!("matched above")
                };
                Expr::Coalesce(vec![
                    Expr::Lift(Box::new(Expr::Binary {
                        op: crate::typing::negate_comparison(op),
                        left,
                        right,
                    })),
                    boolean(!x),
                ])
            }
            other => Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(other),
            },
        }
    }

    /// Fold `items` into nested binary calls of `id`, left to right.
    fn fold(&self, items: Vec<Expr>, id: Option<FunctionId>, plain: fn(Vec<Expr>) -> Expr) -> Expr {
        if id.is_none() {
            return plain(items);
        }
        let mut items = items.into_iter();
        let first = items
            .next()
            .expect("a non-empty conjunction or disjunction");
        items.fold(first, |acc, next| Self::call(id, vec![acc, next], plain))
    }

    /// Normalize `expr` in a predicate position, making it a plain `bool`.
    fn predicate(&self, expr: Expr, scope: &Padded) -> Expr {
        let expr = self.value(expr, scope);
        self.truth(expr, false, scope)
    }

    /// Return a plain `bool` that is `expr`, or `absent` where `expr` is
    /// absent: `(coalesce expr absent)`, pushed through `and3`, `or3`,
    /// `not3` and `try`.
    fn truth(&self, expr: Expr, absent: bool, scope: &Padded) -> Expr {
        let truth = self.truth_unsimplified(expr, absent, scope);
        self.simplify(truth, scope)
    }

    /// Return `truth`'s result before its top is simplified.
    fn truth_unsimplified(&self, expr: Expr, absent: bool, scope: &Padded) -> Expr {
        if !self.optional(&expr, scope) {
            return expr;
        }
        match expr {
            Expr::Call { func, mut args } if Self::is(&func, self.and3) && args.len() == 2 => {
                let q = args.pop().expect("two arguments");
                let p = args.pop().expect("two arguments");
                flatten_and(vec![
                    self.truth(p, absent, scope),
                    self.truth(q, absent, scope),
                ])
            }
            Expr::Call { func, mut args } if Self::is(&func, self.or3) && args.len() == 2 => {
                let q = args.pop().expect("two arguments");
                let p = args.pop().expect("two arguments");
                flatten_or(vec![
                    self.truth(p, absent, scope),
                    self.truth(q, absent, scope),
                ])
            }
            Expr::Call { func, mut args } if Self::is(&func, self.not3) && args.len() == 1 => {
                Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(self.truth(args.pop().expect("one argument"), !absent, scope)),
                }
            }
            Expr::Try { expr, default } => Expr::Try {
                expr: Box::new(self.truth(*expr, absent, scope)),
                default: Box::new(self.truth(*default, absent, scope)),
            },
            other => Expr::Coalesce(vec![other, boolean(absent)]),
        }
    }
}

/// Return the conjunction of `items`, flattening nested conjunctions.
fn flatten_and(items: Vec<Expr>) -> Expr {
    Expr::And(
        items
            .into_iter()
            .flat_map(|e| match e {
                Expr::And(inner) => inner,
                other => vec![other],
            })
            .collect(),
    )
}

/// Return the disjunction of `items`, flattening nested disjunctions.
fn flatten_or(items: Vec<Expr>) -> Expr {
    Expr::Or(
        items
            .into_iter()
            .flat_map(|e| match e {
                Expr::Or(inner) => inner,
                other => vec![other],
            })
            .collect(),
    )
}
