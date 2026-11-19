//! Building expressions.

use std::ops;

use crate::expr::{BinaryOp, CaseBranch, Expr, NamedExpr, SubqueryKind, UnaryOp};
use crate::names::{BindingId, ColumnId, ColumnRef, FunctionRef, Ident};
use crate::rel::{AggCall, NamedAgg, Rel, SortKey};
use crate::types::{DataType, Type};
use crate::value::Value;

use super::rel::RelBuilder;

/// Construct a named column reference from `name` or `qualifier.name`.
#[must_use]
pub fn col(path: &str) -> Expr {
    Expr::Column(ColumnRef::named(path))
}

/// Construct a resolved column reference.
#[must_use]
pub fn bound(binding: u32, column: u32) -> Expr {
    Expr::Column(ColumnRef::bound(
        BindingId::new(binding),
        ColumnId::new(column),
    ))
}

/// Construct a literal.
///
/// # Examples
///
/// ```
/// use interlaken_ir::builder::lit;
/// use interlaken_ir::{Expr, Value};
///
/// assert_eq!(lit(5i32), Expr::Literal(Value::Int4(5)));
/// ```
#[must_use]
pub fn lit(value: impl Into<Value>) -> Expr {
    Expr::Literal(value.into())
}

/// Construct an untyped NULL literal.
#[must_use]
pub fn null() -> Expr {
    Expr::Literal(Value::Null)
}

/// Construct the typed parameter `$index` (1-based).
#[must_use]
pub fn param(index: u32, ty: Type) -> Expr {
    Expr::Param { index, ty }
}

/// Construct a scalar function call.
#[must_use]
pub fn call(func: impl Into<FunctionRef>, args: impl IntoIterator<Item = Expr>) -> Expr {
    Expr::Call {
        func: func.into(),
        args: args.into_iter().collect(),
    }
}

/// Construct an n-ary conjunction.
#[must_use]
pub fn and(items: impl IntoIterator<Item = Expr>) -> Expr {
    Expr::And(items.into_iter().collect())
}

/// Construct an n-ary disjunction.
#[must_use]
pub fn or(items: impl IntoIterator<Item = Expr>) -> Expr {
    Expr::Or(items.into_iter().collect())
}

/// Construct `COALESCE(items...)`.
#[must_use]
pub fn coalesce(items: impl IntoIterator<Item = Expr>) -> Expr {
    Expr::Coalesce(items.into_iter().collect())
}

/// Construct an array of `items`; an empty array needs a cast to its type.
#[must_use]
pub fn array(items: impl IntoIterator<Item = Expr>) -> Expr {
    Expr::Array(items.into_iter().collect())
}

/// Construct `TRY(expr, default)`: `default` when `expr` raises an error.
/// Privileged: only engine passes may use it.
#[must_use]
pub fn try_or(expr: Expr, default: Expr) -> Expr {
    Expr::Try {
        expr: Box::new(expr),
        default: Box::new(default),
    }
}

/// Start a searched `CASE WHEN ... THEN ...`.
pub fn case() -> Case {
    Case {
        operand: None,
        branches: Vec::new(),
    }
}

/// Start a simple `CASE operand WHEN value THEN ...`.
pub fn case_on(operand: Expr) -> Case {
    Case {
        operand: Some(Box::new(operand)),
        branches: Vec::new(),
    }
}

/// Construct `EXISTS (rel)`.
#[must_use]
pub fn exists(rel: impl Into<Rel>) -> Expr {
    Expr::Subquery {
        kind: SubqueryKind::Exists,
        rel: Box::new(rel.into()),
    }
}

/// Construct a scalar subquery.
#[must_use]
pub fn scalar(rel: impl Into<Rel>) -> Expr {
    Expr::Subquery {
        kind: SubqueryKind::Scalar,
        rel: Box::new(rel.into()),
    }
}

/// Construct an aggregate call; chain [`AggCall::distinct`], [`AggCall::filter`] and [`AggCall::alias`].
#[must_use]
pub fn agg(func: impl Into<FunctionRef>, args: impl IntoIterator<Item = Expr>) -> AggCall {
    AggCall {
        func: func.into(),
        args: args.into_iter().collect(),
        distinct: false,
        filter: None,
    }
}

/// Construct `count(*)`.
#[must_use]
pub fn count_star() -> AggCall {
    agg("count", [])
}

/// A `CASE` expression under construction.
#[derive(Clone, Debug)]
#[must_use]
pub struct Case {
    /// The operand of a simple `CASE`.
    operand: Option<Box<Expr>>,
    /// The branches so far.
    branches: Vec<CaseBranch>,
}

impl Case {
    /// Append a `WHEN when THEN then` branch.
    pub fn when(mut self, when: Expr, then: Expr) -> Self {
        self.branches.push(CaseBranch { when, then });
        self
    }

    /// Finish with an `ELSE` result.
    #[must_use]
    pub fn otherwise(self, otherwise: Expr) -> Expr {
        Expr::Case {
            operand: self.operand,
            branches: self.branches,
            otherwise: Some(Box::new(otherwise)),
        }
    }

    /// Finish without an `ELSE`, so unmatched rows give NULL.
    #[must_use]
    pub fn end(self) -> Expr {
        Expr::Case {
            operand: self.operand,
            branches: self.branches,
            otherwise: None,
        }
    }
}

impl AggCall {
    /// Return a copy that aggregates distinct argument values only.
    #[must_use]
    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    /// Return a copy that skips rows where `predicate` is not TRUE.
    #[must_use]
    pub fn filter(mut self, predicate: Expr) -> Self {
        self.filter = Some(predicate);
        self
    }

    /// Name the aggregate's output column.
    #[must_use]
    pub fn alias(self, name: impl Into<Ident>) -> NamedAgg {
        NamedAgg {
            agg: self,
            name: Some(name.into()),
        }
    }
}

/// Construct a binary operator node.
fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

/// Construct a unary operator node.
fn unary(op: UnaryOp, expr: Expr) -> Expr {
    Expr::Unary {
        op,
        expr: Box::new(expr),
    }
}

/// Construct a quantified subquery comparison.
fn quantified(rel: impl Into<Rel>, kind: SubqueryKind) -> Expr {
    Expr::Subquery {
        kind,
        rel: Box::new(rel.into()),
    }
}

impl Expr {
    /// Construct `self = other`.
    #[must_use]
    pub fn eq(self, other: Expr) -> Expr {
        binary(BinaryOp::Eq, self, other)
    }

    /// Construct `self <> other`.
    #[must_use]
    pub fn ne(self, other: Expr) -> Expr {
        binary(BinaryOp::Ne, self, other)
    }

    /// Construct `self < other`.
    #[must_use]
    pub fn lt(self, other: Expr) -> Expr {
        binary(BinaryOp::Lt, self, other)
    }

    /// Construct `self <= other`.
    #[must_use]
    pub fn le(self, other: Expr) -> Expr {
        binary(BinaryOp::Le, self, other)
    }

    /// Construct `self > other`.
    #[must_use]
    pub fn gt(self, other: Expr) -> Expr {
        binary(BinaryOp::Gt, self, other)
    }

    /// Construct `self >= other`.
    #[must_use]
    pub fn ge(self, other: Expr) -> Expr {
        binary(BinaryOp::Ge, self, other)
    }

    /// Construct `self || other`.
    #[must_use]
    pub fn concat(self, other: Expr) -> Expr {
        binary(BinaryOp::Concat, self, other)
    }

    /// Construct `self LIKE pattern`.
    #[must_use]
    pub fn like(self, pattern: Expr) -> Expr {
        binary(BinaryOp::Like, self, pattern)
    }

    /// Construct `self IS DISTINCT FROM other`.
    #[must_use]
    pub fn is_distinct_from(self, other: Expr) -> Expr {
        binary(BinaryOp::IsDistinctFrom, self, other)
    }

    /// Construct `self IS NOT DISTINCT FROM other`.
    #[must_use]
    pub fn is_not_distinct_from(self, other: Expr) -> Expr {
        binary(BinaryOp::IsNotDistinctFrom, self, other)
    }

    /// Construct `self IS NULL`.
    #[must_use]
    pub fn is_null(self) -> Expr {
        unary(UnaryOp::IsNull, self)
    }

    /// Construct `self IS NOT NULL`.
    #[must_use]
    pub fn is_not_null(self) -> Expr {
        unary(UnaryOp::IsNotNull, self)
    }

    /// Construct `self AND other`, extending `self` if it is already a conjunction.
    #[must_use]
    pub fn and(self, other: Expr) -> Expr {
        match self {
            Expr::And(mut items) => {
                items.push(other);
                Expr::And(items)
            }
            first => Expr::And(vec![first, other]),
        }
    }

    /// Construct `self OR other`, extending `self` if it is already a disjunction.
    #[must_use]
    pub fn or(self, other: Expr) -> Expr {
        match self {
            Expr::Or(mut items) => {
                items.push(other);
                Expr::Or(items)
            }
            first => Expr::Or(vec![first, other]),
        }
    }

    /// Construct `NULLIF(self, other)`.
    #[must_use]
    pub fn nullif(self, other: Expr) -> Expr {
        Expr::NullIf {
            left: Box::new(self),
            right: Box::new(other),
        }
    }

    /// Construct an explicit cast.
    #[must_use]
    pub fn cast(self, to: impl Into<DataType>) -> Expr {
        Expr::Cast {
            expr: Box::new(self),
            to: to.into(),
        }
    }

    /// Construct `self IN (rel)`.
    #[must_use]
    pub fn in_subquery(self, rel: impl Into<Rel>) -> Expr {
        quantified(rel, SubqueryKind::In(Box::new(self)))
    }

    /// Construct `self op ANY (rel)`; `op` should be a comparison.
    #[must_use]
    pub fn any(self, op: BinaryOp, rel: impl Into<Rel>) -> Expr {
        quantified(
            rel,
            SubqueryKind::Any {
                op,
                left: Box::new(self),
            },
        )
    }

    /// Construct `self op ALL (rel)`; `op` should be a comparison.
    #[must_use]
    pub fn all(self, op: BinaryOp, rel: impl Into<Rel>) -> Expr {
        quantified(
            rel,
            SubqueryKind::All {
                op,
                left: Box::new(self),
            },
        )
    }

    /// Name the expression's output column.
    #[must_use]
    pub fn alias(self, name: impl Into<Ident>) -> NamedExpr {
        NamedExpr::new(self, name)
    }

    /// Construct an ascending sort key, NULLs last.
    #[must_use]
    pub fn asc(self) -> SortKey {
        SortKey::asc(self)
    }

    /// Construct a descending sort key, NULLs first.
    #[must_use]
    pub fn desc(self) -> SortKey {
        SortKey::desc(self)
    }
}

/// Implement an arithmetic operator trait for `Expr` as a binary node.
macro_rules! arithmetic {
    ($trait:ident, $method:ident, $op:ident) => {
        impl ops::$trait for Expr {
            type Output = Expr;

            /// Construct the arithmetic expression.
            fn $method(self, other: Expr) -> Expr {
                binary(BinaryOp::$op, self, other)
            }
        }
    };
}

arithmetic!(Add, add, Add);
arithmetic!(Sub, sub, Sub);
arithmetic!(Mul, mul, Mul);
arithmetic!(Div, div, Div);
arithmetic!(Rem, rem, Mod);

impl ops::Neg for Expr {
    type Output = Expr;

    /// Construct arithmetic negation.
    fn neg(self) -> Expr {
        unary(UnaryOp::Neg, self)
    }
}

impl ops::Not for Expr {
    type Output = Expr;

    /// Construct boolean `NOT`.
    fn not(self) -> Expr {
        unary(UnaryOp::Not, self)
    }
}

impl From<RelBuilder> for Rel {
    /// Finish the builder.
    fn from(builder: RelBuilder) -> Self {
        builder.build()
    }
}
