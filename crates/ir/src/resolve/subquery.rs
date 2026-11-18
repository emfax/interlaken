//! Lowering subqueries to `Apply` (ADR-0010, ADR-0012).

use super::api::Resolver;
use super::coerce::{Typed, typed_null};
use super::error::ResolveError;
use super::expr::{ExprCx, Lowered};
use super::scope::{Scope, ScopeColumn};
use crate::catalog::FunctionKind;
use crate::expr::{BinaryOp, CaseBranch, Expr, NamedExpr, SubqueryKind, UnaryOp};
use crate::names::{ColumnRef, FunctionRef};
use crate::rel::{AggCall, ApplyKind, NamedAgg, Rel};
use crate::types::{DataType, ScalarType};
use crate::typing;
use crate::value::Value;

/// Return a reference to a scope column.
fn column_ref(c: &ScopeColumn) -> Expr {
    Expr::Column(ColumnRef::bound(c.binding, c.column))
}

/// Return the subquery's only visible column.
fn single_column(scope: &Scope) -> Result<ScopeColumn, ResolveError> {
    let mut visible = scope.visible();
    match (visible.next(), visible.next()) {
        (Some(c), None) => Ok(c.clone()),
        _ => Err(ResolveError::Invalid(format!(
            "a subquery used as a value must return one column, not {}",
            scope.visible().count()
        ))),
    }
}

/// Return the boolean literal `value`.
fn boolean(value: bool) -> Expr {
    Expr::Literal(Value::Bool(value))
}

impl Resolver<'_> {
    /// Resolve `rel` as a subquery seeing the scopes of `cx`.
    fn subquery(&mut self, rel: &Rel, cx: &ExprCx<'_>) -> Result<(Rel, Scope), ResolveError> {
        self.rel(rel, &cx.env)
    }

    /// Compare `left` with the subquery column `column` by `op`.
    fn compare(
        &mut self,
        op: BinaryOp,
        left: Typed,
        column: &ScopeColumn,
    ) -> Result<Expr, ResolveError> {
        let right = Typed {
            expr: column_ref(column),
            ty: Some(column.ty.clone()),
        };
        Ok(self.binary(op, left, right)?.expr)
    }

    /// Look up the built-in aggregate `name` with exactly these arguments.
    fn aggregate(&self, name: &str, args: &[DataType]) -> Result<FunctionRef, ResolveError> {
        self.catalog
            .function_by_name(&name.into(), args)
            .filter(|f| f.kind == FunctionKind::Aggregate)
            .map(|f| FunctionRef::Id(f.id))
            .ok_or_else(|| {
                ResolveError::UnknownFunction(format!(
                    "{name}, a built-in aggregate needed to lower this subquery"
                ))
            })
    }

    /// Lower `left op ANY (rel)` anywhere in an expression, with three-valued logic.
    ///
    /// The subquery becomes an aggregate computing, over its rows, whether
    /// any comparison is TRUE, how many rows there are, and how many
    /// comparisons are not NULL. The result is TRUE if any comparison is,
    /// NULL if none is but some are NULL, and FALSE otherwise (including
    /// for no rows).
    fn lower_any(
        &mut self,
        op: BinaryOp,
        left: Typed,
        rel: &Rel,
        cx: &mut ExprCx<'_>,
    ) -> Result<Typed, ResolveError> {
        let (inner, scope) = self.subquery(rel, cx)?;
        let column = single_column(&scope)?;
        let comparison = self.compare(op, left, &column)?;
        let bool_type = DataType::Scalar(ScalarType::Bool);
        let agg = |func, args: Vec<Expr>, name: &str| NamedAgg {
            agg: AggCall {
                func,
                args,
                distinct: false,
                filter: None,
            },
            name: Some(name.into()),
        };
        let aggs = vec![
            agg(
                self.aggregate("bool_or", std::slice::from_ref(&bool_type))?,
                vec![comparison.clone()],
                "any_true",
            ),
            agg(self.aggregate("count", &[])?, vec![], "rows"),
            agg(
                self.aggregate("count", &[bool_type])?,
                vec![comparison],
                "known",
            ),
        ];
        let binding = self.fresh()?;
        let count = DataType::Scalar(ScalarType::Int8).not_null();
        let agg_scope = Scope::derived(
            binding,
            None,
            [
                (Some("any_true".into()), typing::boolean(true)),
                (Some("rows".into()), count.clone()),
                (Some("known".into()), count),
            ],
        )?;
        let [any_true, rows, known] = [0, 1, 2].map(|i| column_ref(&agg_scope.columns[i]));
        cx.lowered.push(Lowered {
            kind: ApplyKind::Single,
            inner: Rel::Aggregate {
                input: Box::new(inner),
                keys: Vec::new(),
                aggs,
                binding: Some(binding),
            },
            scope: agg_scope,
        });
        let expr = Expr::Case {
            operand: None,
            branches: vec![
                CaseBranch {
                    when: any_true,
                    then: boolean(true),
                },
                CaseBranch {
                    when: Expr::Binary {
                        op: BinaryOp::Gt,
                        left: Box::new(rows),
                        right: Box::new(known),
                    },
                    then: typed_null(DataType::Scalar(ScalarType::Bool)).expr,
                },
            ],
            otherwise: Some(Box::new(boolean(false))),
        };
        Ok(Typed {
            expr,
            ty: Some(typing::boolean(true)),
        })
    }

    /// Lower a subquery in a general expression position.
    pub(crate) fn lower_subquery(
        &mut self,
        kind: &SubqueryKind,
        rel: &Rel,
        cx: &mut ExprCx<'_>,
    ) -> Result<Typed, ResolveError> {
        match kind {
            SubqueryKind::Scalar => {
                let (inner, scope) = self.subquery(rel, cx)?;
                let column = single_column(&scope)?;
                cx.lowered.push(Lowered {
                    kind: ApplyKind::Single,
                    inner,
                    scope,
                });
                Ok(Typed {
                    expr: column_ref(&column),
                    ty: Some(column.ty.with_nullable(true)),
                })
            }
            SubqueryKind::Exists => {
                // A left apply of at most one marker row: present exactly when a row exists.
                let (inner, _) = self.subquery(rel, cx)?;
                let binding = self.fresh()?;
                let scope = Scope::derived(
                    binding,
                    None,
                    [(Some("present".into()), typing::boolean(false))],
                )?;
                let marker = column_ref(&scope.columns[0]);
                cx.lowered.push(Lowered {
                    kind: ApplyKind::Left,
                    inner: Rel::Project {
                        input: Box::new(Rel::Limit {
                            input: Box::new(inner),
                            limit: Some(Expr::Literal(Value::Int8(1))),
                            offset: None,
                        }),
                        exprs: vec![NamedExpr::new(boolean(true), "present")],
                        binding: Some(binding),
                    },
                    scope,
                });
                Ok(Typed {
                    expr: Expr::Unary {
                        op: UnaryOp::IsNotNull,
                        expr: Box::new(marker),
                    },
                    ty: Some(typing::boolean(false)),
                })
            }
            SubqueryKind::In(left) => {
                let left = self.expr(left, cx)?;
                self.lower_any(BinaryOp::Eq, left, rel, cx)
            }
            SubqueryKind::Any { op, left } => {
                let left = self.expr(left, cx)?;
                self.lower_any(*op, left, rel, cx)
            }
            SubqueryKind::All { op, left } => {
                // x op ALL (s) is NOT (x negated-op ANY (s)), which holds in three-valued logic.
                let left = self.expr(left, cx)?;
                let any = self.lower_any(typing::negate_comparison(*op), left, rel, cx)?;
                Ok(Typed {
                    expr: Expr::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(any.expr),
                    },
                    ty: any.ty,
                })
            }
        }
    }

    /// Lower `conjunct` to a semi or anti apply if it is a top-level filter
    /// conjunct of a form that allows it; return whether it was.
    ///
    /// In a filter, NULL and FALSE both reject a row, so `EXISTS`, `IN`
    /// and `op ANY` reduce to a semi apply (with the comparison as a filter
    /// on the subquery) and `NOT EXISTS` to an anti apply. `NOT IN` and
    /// `ALL` are not handled here, because a NULL comparison must reject.
    pub(crate) fn lower_conjunct(
        &mut self,
        conjunct: &Expr,
        cx: &mut ExprCx<'_>,
    ) -> Result<bool, ResolveError> {
        let (kind, rel, comparison) = match conjunct {
            Expr::Subquery {
                kind: SubqueryKind::Exists,
                rel,
            } => (ApplyKind::Semi, rel, None),
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
            } => match expr.as_ref() {
                Expr::Subquery {
                    kind: SubqueryKind::Exists,
                    rel,
                } => (ApplyKind::Anti, rel, None),
                _ => return Ok(false),
            },
            Expr::Subquery {
                kind: SubqueryKind::In(left),
                rel,
            } => (ApplyKind::Semi, rel, Some((BinaryOp::Eq, left))),
            Expr::Subquery {
                kind: SubqueryKind::Any { op, left },
                rel,
            } => (ApplyKind::Semi, rel, Some((*op, left))),
            _ => return Ok(false),
        };
        let left = comparison
            .map(|(op, left)| self.expr(left, cx).map(|l| (op, l)))
            .transpose()?;
        let (inner, scope) = self.subquery(rel, cx)?;
        let inner = match left {
            None => inner,
            Some((op, left)) => {
                let column = single_column(&scope)?;
                Rel::Filter {
                    input: Box::new(inner),
                    predicate: self.compare(op, left, &column)?,
                }
            }
        };
        cx.lowered.push(Lowered { kind, inner, scope });
        Ok(true)
    }
}
