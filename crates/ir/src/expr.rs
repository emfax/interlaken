//! Scalar expressions (ADR-0010).

use crate::names::{ColumnRef, FunctionRef, Ident};
use crate::rel::Rel;
use crate::types::{DataType, Type};
use crate::value::Value;

/// A scalar expression.
///
/// Aggregate calls are not expressions: they appear only in
/// [`Rel::Aggregate`], so "aggregates only inside Aggregate" holds by
/// construction. Subqueries appear only in unresolved IR; the resolver
/// lowers them to [`Rel::Apply`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Expr {
    /// A column reference.
    Column(ColumnRef),
    /// A literal.
    Literal(Value),
    /// A typed parameter, `$index` (1-based), for prepared statements.
    Param {
        /// The 1-based parameter position.
        index: u32,
        /// The parameter's type.
        ty: Type,
    },
    /// A scalar function call.
    Call {
        /// The function.
        func: FunctionRef,
        /// The arguments.
        args: Vec<Expr>,
    },
    /// A unary operator.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        expr: Box<Expr>,
    },
    /// A binary operator.
    Binary {
        /// The operator.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
    /// Conjunction with three-valued logic; `true` when empty.
    And(Vec<Expr>),
    /// Disjunction with three-valued logic; `false` when empty.
    Or(Vec<Expr>),
    /// `CASE`. Branches not taken are not evaluated and must not raise errors.
    Case {
        /// The operand of a simple `CASE x WHEN v THEN ...`; `None` for a searched `CASE`.
        operand: Option<Box<Expr>>,
        /// The `WHEN` branches, in order.
        branches: Vec<CaseBranch>,
        /// The `ELSE` result; NULL when absent.
        otherwise: Option<Box<Expr>>,
    },
    /// `COALESCE`: the first non-NULL argument, evaluated left to right.
    Coalesce(Vec<Expr>),
    /// An array of the elements, which share one type. An empty array has
    /// no element type, so resolved IR writes it under a cast, as
    /// `(cast (array) T)`, the way it writes a typed NULL.
    Array(Vec<Expr>),
    /// `NULLIF(left, right)`.
    NullIf {
        /// The value returned unless it equals `right`.
        left: Box<Expr>,
        /// The value compared against.
        right: Box<Expr>,
    },
    /// An explicit cast; the result's nullability follows the operand's.
    Cast {
        /// The operand.
        expr: Box<Expr>,
        /// The target type.
        to: DataType,
    },
    /// A subquery (unresolved IR only).
    Subquery {
        /// How the subquery's rows are used.
        kind: SubqueryKind,
        /// The subquery.
        rel: Box<Rel>,
    },
}

/// A unary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// Boolean `NOT`, with three-valued logic.
    Not,
    /// Arithmetic negation; overflow is an error.
    Neg,
    /// `IS NULL`.
    IsNull,
    /// `IS NOT NULL`.
    IsNotNull,
}

/// A binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    /// `=`.
    Eq,
    /// `<>`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `+`; overflow is an error.
    Add,
    /// `-`; overflow is an error.
    Sub,
    /// `*`; overflow is an error.
    Mul,
    /// `/`; division by zero is an error.
    Div,
    /// `%`; division by zero is an error.
    Mod,
    /// String concatenation, `||`.
    Concat,
    /// `LIKE`, with `\` as the escape character.
    Like,
    /// `IS DISTINCT FROM`.
    IsDistinctFrom,
    /// `IS NOT DISTINCT FROM`.
    IsNotDistinctFrom,
}

impl BinaryOp {
    /// Return `true` for `=`, `<>`, `<`, `<=`, `>` and `>=`.
    #[must_use]
    pub const fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge
        )
    }
}

/// One `WHEN ... THEN ...` branch of a `CASE`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CaseBranch {
    /// The condition, or the value compared with the operand in a simple `CASE`.
    pub when: Expr,
    /// The result when the branch is taken.
    pub then: Expr,
}

/// How a subquery's rows are used.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SubqueryKind {
    /// A scalar subquery: one column, at most one row (NULL when empty).
    Scalar,
    /// `EXISTS (subquery)`.
    Exists,
    /// `expr IN (subquery)`.
    In(Box<Expr>),
    /// `expr op ANY (subquery)`; `op` is a comparison.
    Any {
        /// The comparison.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr>,
    },
    /// `expr op ALL (subquery)`; `op` is a comparison.
    All {
        /// The comparison.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr>,
    },
}

/// An expression with an optional output column name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NamedExpr {
    /// The expression.
    pub expr: Expr,
    /// The output column name, if given.
    pub name: Option<Ident>,
}

impl NamedExpr {
    /// Construct a named expression.
    #[must_use]
    pub fn new(expr: Expr, name: impl Into<Ident>) -> Self {
        Self {
            expr,
            name: Some(name.into()),
        }
    }
}

impl From<Expr> for NamedExpr {
    /// Wrap an expression without an output name.
    fn from(expr: Expr) -> Self {
        Self { expr, name: None }
    }
}

impl Expr {
    /// Return the direct sub-expressions, in evaluation order.
    ///
    /// Expressions inside a subquery's relation are not included; reach
    /// them through [`Rel::exprs`].
    #[must_use]
    pub fn children(&self) -> Vec<&Expr> {
        match self {
            Self::Column(_) | Self::Literal(_) | Self::Param { .. } => Vec::new(),
            Self::Call { args, .. } => args.iter().collect(),
            Self::Unary { expr, .. } | Self::Cast { expr, .. } => vec![expr],
            Self::Binary { left, right, .. } | Self::NullIf { left, right } => vec![left, right],
            Self::And(items) | Self::Or(items) | Self::Coalesce(items) | Self::Array(items) => {
                items.iter().collect()
            }
            Self::Case {
                operand,
                branches,
                otherwise,
            } => operand
                .iter()
                .map(Box::as_ref)
                .chain(branches.iter().flat_map(|b| [&b.when, &b.then]))
                .chain(otherwise.iter().map(Box::as_ref))
                .collect(),
            Self::Subquery { kind, .. } => match kind {
                SubqueryKind::Scalar | SubqueryKind::Exists => Vec::new(),
                SubqueryKind::In(left)
                | SubqueryKind::Any { left, .. }
                | SubqueryKind::All { left, .. } => vec![left],
            },
        }
    }

    /// Return the direct sub-expressions mutably, in the same order as [`Expr::children`].
    pub fn children_mut(&mut self) -> Vec<&mut Expr> {
        match self {
            Self::Column(_) | Self::Literal(_) | Self::Param { .. } => Vec::new(),
            Self::Call { args, .. } => args.iter_mut().collect(),
            Self::Unary { expr, .. } | Self::Cast { expr, .. } => vec![expr],
            Self::Binary { left, right, .. } | Self::NullIf { left, right } => vec![left, right],
            Self::And(items) | Self::Or(items) | Self::Coalesce(items) | Self::Array(items) => {
                items.iter_mut().collect()
            }
            Self::Case {
                operand,
                branches,
                otherwise,
            } => operand
                .iter_mut()
                .map(Box::as_mut)
                .chain(branches.iter_mut().flat_map(|b| [&mut b.when, &mut b.then]))
                .chain(otherwise.iter_mut().map(Box::as_mut))
                .collect(),
            Self::Subquery { kind, .. } => match kind {
                SubqueryKind::Scalar | SubqueryKind::Exists => Vec::new(),
                SubqueryKind::In(left)
                | SubqueryKind::Any { left, .. }
                | SubqueryKind::All { left, .. } => vec![left],
            },
        }
    }
}
