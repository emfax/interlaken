//! Resolving expressions.

use std::sync::Arc;

use super::api::Resolver;
use super::coerce::{Mode, Typed, coerce, predicate, settle, unify};
use super::error::ResolveError;
use super::scope::Scope;
use crate::catalog::{FunctionDef, FunctionKind};
use crate::expr::{BinaryOp, CaseBranch, Expr, UnaryOp};
use crate::names::{ColumnRef, FunctionRef, Ident};
use crate::rel::{ApplyKind, Rel};
use crate::text::print_expr;
use crate::types::{DataType, ScalarType};
use crate::typing;
use crate::value::Value;

/// A subquery lowered to an `Apply` over the operator's input.
pub(crate) struct Lowered {
    /// The apply kind.
    pub(crate) kind: ApplyKind,
    /// The inner relation.
    pub(crate) inner: Rel,
    /// The inner relation's columns.
    pub(crate) scope: Scope,
}

/// Where an expression is being resolved.
pub(crate) struct ExprCx<'e> {
    /// Visible columns, innermost scope first.
    pub(crate) env: Vec<&'e Scope>,
    /// Subqueries lowered so far, to wrap the operator's input in.
    pub(crate) lowered: Vec<Lowered>,
    /// Why subqueries are not allowed here, if they are not.
    pub(crate) no_subqueries: Option<&'static str>,
    /// Inside a branch that may not be evaluated (`CASE`, `COALESCE`).
    pub(crate) conditional: bool,
}

impl<'e> ExprCx<'e> {
    /// Construct a context where subqueries are lowered into the operator's input.
    pub(crate) fn lowering(env: Vec<&'e Scope>) -> Self {
        Self {
            env,
            lowered: Vec::new(),
            no_subqueries: None,
            conditional: false,
        }
    }

    /// Construct a context where subqueries are rejected, because of `reason`.
    pub(crate) fn plain(env: Vec<&'e Scope>, reason: &'static str) -> Self {
        Self {
            env,
            lowered: Vec::new(),
            no_subqueries: Some(reason),
            conditional: false,
        }
    }
}

/// Return the name an output column takes from `original` if it has no `as` name.
///
/// A column reference keeps its column's name; other expressions are unnamed.
pub(crate) fn derived_name(original: &Expr, resolved: &Expr, env: &[&Scope]) -> Option<Ident> {
    match (original, resolved) {
        (Expr::Column(_), Expr::Column(ColumnRef::Bound { binding, column })) => env
            .iter()
            .find_map(|s| s.lookup_resolved(*binding, *column))
            .and_then(|c| c.name.clone()),
        _ => None,
    }
}

/// Return the cost of passing `arg` where `param` is expected, or `None` if impossible.
///
/// Exact matches and untyped NULLs cost nothing; a bare integer literal
/// costs nothing as `int8` and one as any other numeric it fits; other
/// implicit conversions cost one.
fn conversion_cost(arg: &Typed, param: &DataType) -> Option<u32> {
    let Some(ty) = &arg.ty else { return Some(0) };
    if &ty.data == param {
        return Some(0);
    }
    if let Expr::Literal(Value::Int8(_)) = arg.expr {
        return coerce(arg.clone(), param, Mode::Implicit).ok().map(|_| 1);
    }
    typing::implicit(&ty.data, param).then_some(1)
}

impl Resolver<'_> {
    /// Choose the overload of `func` of `kind` that best fits `args`, and convert the arguments.
    pub(crate) fn pick_function(
        &self,
        func: &FunctionRef,
        args: Vec<Typed>,
        kind: FunctionKind,
    ) -> Result<(Arc<FunctionDef>, Vec<Expr>), ResolveError> {
        let candidates = match func {
            FunctionRef::Named(name) => self.catalog.functions_by_name(name),
            FunctionRef::Id(id) => self.catalog.function_by_id(*id).into_iter().collect(),
        };
        let describe = || {
            let types: Vec<String> = args
                .iter()
                .map(|a| {
                    a.ty.as_ref()
                        .map_or_else(|| "unknown".into(), |t| t.data.to_string())
                })
                .collect();
            let name = match func {
                FunctionRef::Named(name) => name.to_string(),
                FunctionRef::Id(id) => format!("(fid {})", id.get()),
            };
            format!("{name}({})", types.join(", "))
        };
        let mut best: Vec<(u32, Arc<FunctionDef>)> = Vec::new();
        let mut wrong_kind = None;
        for candidate in candidates {
            if candidate.args.len() != args.len() {
                continue;
            }
            if candidate.kind != kind {
                wrong_kind = Some(candidate.kind);
                continue;
            }
            let cost: Option<u32> = args
                .iter()
                .zip(&candidate.args)
                .map(|(a, p)| conversion_cost(a, p))
                .sum();
            if let Some(cost) = cost {
                if best.first().is_none_or(|(c, _)| cost < *c) {
                    best = vec![(cost, candidate)];
                } else if best.first().is_some_and(|(c, _)| cost == *c) {
                    best.push((cost, candidate));
                }
            }
        }
        match best.len() {
            1 => {
                let def = best.pop().expect("one candidate").1;
                let exprs = args
                    .into_iter()
                    .zip(&def.args)
                    .map(|(a, p)| coerce(a, p, Mode::Implicit).map(|t| t.expr))
                    .collect::<Result<_, _>>()?;
                Ok((def, exprs))
            }
            0 => match wrong_kind {
                Some(FunctionKind::Aggregate) => Err(ResolveError::Invalid(format!(
                    "aggregate {} called outside an aggregate",
                    describe()
                ))),
                Some(FunctionKind::Scalar) => Err(ResolveError::Invalid(format!(
                    "{} is not an aggregate",
                    describe()
                ))),
                None => Err(ResolveError::UnknownFunction(describe())),
            },
            _ => Err(ResolveError::AmbiguousFunction(describe())),
        }
    }

    /// Resolve each expression in `items` in order.
    fn exprs(&mut self, items: &[Expr], cx: &mut ExprCx<'_>) -> Result<Vec<Typed>, ResolveError> {
        items.iter().map(|e| self.expr(e, cx)).collect()
    }

    /// Resolve `expr` in a branch that might not be evaluated.
    fn conditional(&mut self, expr: &Expr, cx: &mut ExprCx<'_>) -> Result<Typed, ResolveError> {
        let was = std::mem::replace(&mut cx.conditional, true);
        let result = self.expr(expr, cx);
        cx.conditional = was;
        result
    }

    /// Resolve a column reference against the scopes in `cx`.
    fn column(&self, column: &ColumnRef, cx: &ExprCx<'_>) -> Result<Typed, ResolveError> {
        let found = match column {
            ColumnRef::Named { qualifier, name } => {
                let mut found = None;
                for scope in &cx.env {
                    if let Some(c) = scope.lookup_named(qualifier.as_ref(), name)? {
                        found = Some(c);
                        break;
                    }
                }
                found.ok_or_else(|| {
                    ResolveError::UnknownColumn(
                        qualifier
                            .as_ref()
                            .map_or_else(|| name.to_string(), |q| format!("{q}.{name}")),
                    )
                })?
            }
            ColumnRef::Bound { binding, column } => cx
                .env
                .iter()
                .find_map(|s| s.lookup_bound(*binding, *column))
                .ok_or_else(|| {
                    ResolveError::UnknownColumn(format!("#{}.{}", binding.get(), column.get()))
                })?,
        };
        Ok(Typed {
            expr: Expr::Column(ColumnRef::bound(found.binding, found.column)),
            ty: Some(found.ty.clone()),
        })
    }

    /// Resolve a binary operator applied to resolved operands.
    pub(crate) fn binary(
        &mut self,
        op: BinaryOp,
        left: Typed,
        right: Typed,
    ) -> Result<Typed, ResolveError> {
        let date = DataType::Scalar(ScalarType::Date);
        let (left, right) = if matches!(op, BinaryOp::Add | BinaryOp::Sub)
            && left.ty.as_ref().is_some_and(|t| t.data == date)
            && right
                .ty
                .as_ref()
                .is_some_and(|t| typing::is_integer(&t.data))
        {
            // date ± integer: the integer must be int4.
            (
                left,
                coerce(right, &DataType::Scalar(ScalarType::Int4), Mode::Implicit)?,
            )
        } else {
            let default = match op {
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                    ScalarType::Int8
                }
                _ => ScalarType::Text,
            };
            let (mut exprs, ty) = unify(vec![left, right], default)?;
            let right = exprs.pop().expect("two operands");
            let left = exprs.pop().expect("two operands");
            (
                Typed {
                    expr: left,
                    ty: Some(ty.clone()),
                },
                Typed {
                    expr: right,
                    ty: Some(ty),
                },
            )
        };
        let (l_ty, r_ty) = (
            left.ty.clone().expect("typed"),
            right.ty.clone().expect("typed"),
        );
        let expr = Expr::Binary {
            op,
            left: Box::new(left.expr),
            right: Box::new(right.expr),
        };
        let ty = typing::binary(op, &l_ty, &r_ty)
            .map_err(|e| ResolveError::Type(format!("{e} in {}", print_expr(&expr))))?;
        Ok(Typed { expr, ty: Some(ty) })
    }

    /// Resolve an expression.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn expr(&mut self, expr: &Expr, cx: &mut ExprCx<'_>) -> Result<Typed, ResolveError> {
        Ok(match expr {
            Expr::Column(column) => self.column(column, cx)?,
            Expr::Literal(value) => Typed {
                expr: expr.clone(),
                ty: typing::value_type(value),
            },
            Expr::Param { index, ty } => {
                self.param(*index, ty)?;
                Typed {
                    expr: expr.clone(),
                    ty: Some(ty.clone()),
                }
            }
            Expr::Call { func, args } => {
                let args = self.exprs(args, cx)?;
                let any_nullable = args
                    .iter()
                    .any(|a| a.ty.as_ref().is_none_or(|t| t.nullable));
                let (def, args) = self.pick_function(func, args, FunctionKind::Scalar)?;
                let nullable = def.returns.nullable || (def.properties.strict && any_nullable);
                Typed {
                    expr: Expr::Call {
                        func: FunctionRef::Id(def.id),
                        args,
                    },
                    ty: Some(def.returns.clone().with_nullable(nullable)),
                }
            }
            Expr::Unary { op, expr: operand } => {
                let operand = self.expr(operand, cx)?;
                let (operand, ty) = match op {
                    UnaryOp::Not => {
                        settle(predicate(operand, "the operand of NOT")?, ScalarType::Bool)
                    }
                    UnaryOp::Neg => settle(operand, ScalarType::Int8),
                    UnaryOp::IsNull | UnaryOp::IsNotNull => settle(operand, ScalarType::Text),
                };
                let expr = Expr::Unary {
                    op: *op,
                    expr: Box::new(operand),
                };
                let result = typing::unary(*op, &ty)
                    .map_err(|e| ResolveError::Type(format!("{e} in {}", print_expr(&expr))))?;
                Typed {
                    expr,
                    ty: Some(result),
                }
            }
            Expr::Binary { op, left, right } => {
                let left = self.expr(left, cx)?;
                let right = self.expr(right, cx)?;
                self.binary(*op, left, right)?
            }
            Expr::And(items) | Expr::Or(items) => {
                let mut nullable = false;
                let mut out = Vec::new();
                for item in items {
                    let typed = self.expr(item, cx)?;
                    nullable |= typed.ty.as_ref().is_none_or(|t| t.nullable);
                    out.push(predicate(typed, "an AND or OR operand")?.expr);
                }
                let expr = if matches!(expr, Expr::And(_)) {
                    Expr::And(out)
                } else {
                    Expr::Or(out)
                };
                Typed {
                    expr,
                    ty: Some(typing::boolean(nullable)),
                }
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                if branches.is_empty() {
                    return Err(ResolveError::Invalid(
                        "CASE needs at least one branch".into(),
                    ));
                }
                let operand = operand.as_ref().map(|o| self.expr(o, cx)).transpose()?;
                let mut whens = Vec::new();
                let mut results = Vec::new();
                for (i, branch) in branches.iter().enumerate() {
                    let when = if i == 0 {
                        self.expr(&branch.when, cx)?
                    } else {
                        self.conditional(&branch.when, cx)?
                    };
                    whens.push(when);
                    results.push(self.conditional(&branch.then, cx)?);
                }
                let has_else = otherwise.is_some();
                if let Some(e) = otherwise {
                    results.push(self.conditional(e, cx)?);
                }
                let (mut results, mut result_ty) = unify(results, ScalarType::Text)?;
                if !has_else {
                    result_ty.nullable = true;
                }
                let otherwise = has_else.then(|| Box::new(results.pop().expect("else result")));
                let (operand, whens) = match operand {
                    Some(operand) => {
                        let mut all = vec![operand];
                        all.extend(whens);
                        let (mut exprs, ty) = unify(all, ScalarType::Text)?;
                        if !typing::is_comparable(&ty.data) {
                            return Err(ResolveError::Type(format!(
                                "CASE cannot compare {}",
                                ty.data
                            )));
                        }
                        let operand = exprs.remove(0);
                        (Some(Box::new(operand)), exprs)
                    }
                    None => (
                        None,
                        whens
                            .into_iter()
                            .map(|w| predicate(w, "a CASE condition").map(|t| t.expr))
                            .collect::<Result<_, _>>()?,
                    ),
                };
                Typed {
                    expr: Expr::Case {
                        operand,
                        branches: whens
                            .into_iter()
                            .zip(results)
                            .map(|(when, then)| CaseBranch { when, then })
                            .collect(),
                        otherwise,
                    },
                    ty: Some(result_ty),
                }
            }
            Expr::Coalesce(items) => {
                if items.is_empty() {
                    return Err(ResolveError::Invalid(
                        "COALESCE needs at least one argument".into(),
                    ));
                }
                let mut typed = vec![self.expr(&items[0], cx)?];
                for item in &items[1..] {
                    typed.push(self.conditional(item, cx)?);
                }
                let all_nullable = typed
                    .iter()
                    .all(|t| t.ty.as_ref().is_none_or(|t| t.nullable));
                let (exprs, ty) = unify(typed, ScalarType::Text)?;
                Typed {
                    expr: Expr::Coalesce(exprs),
                    ty: Some(ty.with_nullable(all_nullable)),
                }
            }
            Expr::NullIf { left, right } => {
                let typed = vec![self.expr(left, cx)?, self.expr(right, cx)?];
                let (mut exprs, ty) = unify(typed, ScalarType::Text)?;
                if !typing::is_comparable(&ty.data) {
                    return Err(ResolveError::Type(format!(
                        "NULLIF cannot compare {}",
                        ty.data
                    )));
                }
                let right = exprs.pop().expect("two operands");
                let left = exprs.pop().expect("two operands");
                Typed {
                    expr: Expr::NullIf {
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    ty: Some(ty.with_nullable(true)),
                }
            }
            Expr::Cast { expr: operand, to } => {
                let operand = self.expr(operand, cx)?;
                let nullable = match &operand.ty {
                    None => true,
                    Some(ty) if typing::explicit(&ty.data, to) => ty.nullable,
                    Some(ty) => {
                        return Err(ResolveError::Type(format!(
                            "cannot cast {} to {to}",
                            ty.data
                        )));
                    }
                };
                Typed {
                    expr: Expr::Cast {
                        expr: Box::new(operand.expr),
                        to: to.clone(),
                    },
                    ty: Some(to.clone().nullable().with_nullable(nullable)),
                }
            }
            Expr::Subquery { kind, rel } => {
                if let Some(reason) = cx.no_subqueries {
                    return Err(ResolveError::NotSupported(format!(
                        "subqueries in {reason}"
                    )));
                }
                if cx.conditional {
                    return Err(ResolveError::NotSupported(
                        "subqueries in CASE or COALESCE branches".into(),
                    ));
                }
                self.lower_subquery(kind, rel, cx)?
            }
        })
    }
}
