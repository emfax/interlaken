//! Compiling IR expressions into typed node trees.

use std::collections::BTreeSet;

use arrow_array::ArrayRef;
use interlaken_ir::catalog::Catalog;
use interlaken_ir::interp::{Datum, EvalError, ExecOptions, LikePattern};
use interlaken_ir::verify::expr_type;
use interlaken_ir::{BinaryOp, Expr, UnaryOp};
use interlaken_ir::{BindingId, ColumnId, ColumnRef, DataType, FunctionRef, Type, Value};

use super::eval::State;
use super::input::Input;
use crate::error::ExecError;
use crate::exec::OuterColumn;

/// The built-in functions the kernels implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    /// `lower(text)`.
    Lower,
    /// `upper(text)`.
    Upper,
    /// `length(text)`, in characters.
    Length,
    /// `substr(text, start, length)`, 1-based.
    Substr,
    /// `abs(x)`.
    Abs,
    /// `now()`: the transaction time.
    Now,
    /// `assert_present(x)`: `x`, or an error if it is NULL.
    AssertPresent,
    /// `random()`.
    Random,
    /// `gen_random_uuid()`.
    GenRandomUuid,
    /// `redact(text)`: always `****` (ADR-0032).
    Redact,
    /// `mask_last4(text)`: every character but the last four replaced by `*`.
    MaskLast4,
}

impl Builtin {
    /// Look up a built-in by the function's name.
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "lower" => Self::Lower,
            "upper" => Self::Upper,
            "length" => Self::Length,
            "substr" => Self::Substr,
            "abs" => Self::Abs,
            "now" => Self::Now,
            "assert_present" => Self::AssertPresent,
            "random" => Self::Random,
            "gen_random_uuid" => Self::GenRandomUuid,
            "redact" => Self::Redact,
            "mask_last4" => Self::MaskLast4,
            _ => return None,
        })
    }
}

/// The right-hand side of `LIKE`.
#[derive(Clone, Debug)]
pub(crate) enum Pattern {
    /// A constant pattern, compiled once (or the error compiling it raises).
    Constant(Result<LikePattern, EvalError>),
    /// A constant NULL pattern: every result is NULL.
    Null,
    /// A pattern computed per row.
    Dynamic(Box<Node>),
}

/// What a node computes.
#[derive(Clone, Debug)]
pub(crate) enum Kind {
    /// The input column at this position.
    Column(usize),
    /// A constant (a literal, parameter or typed NULL).
    Constant(Datum),
    /// An expression that raises this error on any row it is evaluated for.
    Failing(EvalError),
    /// A unary operator.
    Unary(UnaryOp, Box<Node>),
    /// A binary operator other than `LIKE`.
    Binary(BinaryOp, Box<Node>, Box<Node>),
    /// `text LIKE pattern`.
    Like(Box<Node>, Pattern),
    /// Conjunction, short-circuiting.
    And(Vec<Node>),
    /// Disjunction, short-circuiting.
    Or(Vec<Node>),
    /// `CASE`, evaluating each branch only for the rows that reach it.
    Case {
        /// The operand of a simple `CASE`.
        operand: Option<Box<Node>>,
        /// The (condition or value, result) branches.
        branches: Vec<(Node, Node)>,
        /// The `ELSE` result.
        otherwise: Option<Box<Node>>,
    },
    /// `COALESCE`, evaluating each argument only for rows still NULL.
    Coalesce(Vec<Node>),
    /// `NULLIF(left, right)`.
    NullIf(Box<Node>, Box<Node>),
    /// An array constructor.
    Array(Vec<Node>),
    /// A cast to the node's type.
    Cast(Box<Node>),
    /// `Try(expr, default)`, containing errors per row.
    Try(Box<Node>, Box<Node>),
    /// A built-in function call.
    Call(Builtin, Vec<Node>),
    /// A call to a function the kernels do not implement; raises if evaluated.
    Unknown(String),
}

/// A compiled expression node.
#[derive(Clone, Debug)]
pub(crate) struct Node {
    /// What it computes.
    pub(crate) kind: Kind,
    /// Its result type.
    pub(crate) ty: DataType,
    /// The input columns the subtree reads, ascending.
    pub(crate) cols: Vec<usize>,
}

/// Compiles expressions against one input layout.
struct Compiler<'a> {
    /// The input columns and their types.
    columns: &'a [(BindingId, ColumnId, Type)],
    /// Columns of enclosing rows, which compile to their values.
    outer: &'a [OuterColumn],
    /// Every visible column with its type: the input's, then the outer ones.
    scope: Vec<(BindingId, ColumnId, Type)>,
    /// The catalog.
    catalog: &'a dyn Catalog,
    /// Parameter values.
    params: &'a [Datum],
    /// Evaluation settings.
    options: ExecOptions,
}

/// Construct an unsupported-expression error.
fn unsupported(what: impl Into<String>) -> ExecError {
    ExecError::Unsupported(what.into())
}

impl Kind {
    /// Return the child nodes.
    fn children(&self) -> Vec<&Node> {
        match self {
            Self::Column(_) | Self::Constant(_) | Self::Failing(_) | Self::Unknown(_) => Vec::new(),
            Self::Unary(_, child) | Self::Cast(child) => vec![child],
            Self::Binary(_, l, r) | Self::NullIf(l, r) | Self::Try(l, r) => vec![l, r],
            Self::Like(text, pattern) => match pattern {
                Pattern::Dynamic(p) => vec![text, p],
                Pattern::Constant(_) | Pattern::Null => vec![text],
            },
            Self::And(items)
            | Self::Or(items)
            | Self::Coalesce(items)
            | Self::Array(items)
            | Self::Call(_, items) => items.iter().collect(),
            Self::Case {
                operand,
                branches,
                otherwise,
            } => operand
                .as_deref()
                .into_iter()
                .chain(branches.iter().flat_map(|(w, t)| [w, t]))
                .chain(otherwise.as_deref())
                .collect(),
        }
    }
}

impl Node {
    /// Construct a node, collecting the columns its subtree reads.
    fn new(kind: Kind, ty: DataType) -> Self {
        let mut cols = BTreeSet::new();
        if let Kind::Column(c) = kind {
            cols.insert(c);
        }
        for child in kind.children() {
            cols.extend(child.cols.iter().copied());
        }
        Self {
            kind,
            ty,
            cols: cols.into_iter().collect(),
        }
    }
}

impl Compiler<'_> {
    /// Return the result type of `expr`.
    fn type_of(&self, expr: &Expr) -> Result<DataType, ExecError> {
        expr_type(expr, self.catalog, &self.scope)
            .map(|t| t.data)
            .map_err(|e| unsupported(format!("expression does not type-check: {e}")))
    }

    /// Compile every expression of a list.
    fn all(&self, items: &[Expr]) -> Result<Vec<Node>, ExecError> {
        items.iter().map(|e| self.compile(e)).collect()
    }

    /// Compile an expression into a boxed node.
    fn boxed(&self, expr: &Expr) -> Result<Box<Node>, ExecError> {
        Ok(Box::new(self.compile(expr)?))
    }

    /// Compile `expr`.
    fn compile(&self, expr: &Expr) -> Result<Node, ExecError> {
        // Typed NULLs and empty arrays have no type of their own; their cast gives it.
        if let Expr::Cast { expr: inner, to } = expr {
            match inner.as_ref() {
                Expr::Literal(Value::Null) => {
                    return Ok(Node::new(Kind::Constant(Datum::Null), to.clone()));
                }
                Expr::Array(items) if items.is_empty() => {
                    return Ok(Node::new(
                        Kind::Constant(Datum::Array(Vec::new())),
                        to.clone(),
                    ));
                }
                _ => {}
            }
        }
        let ty = self.type_of(expr)?;
        let kind = match expr {
            Expr::Column(ColumnRef::Bound { binding, column }) => {
                if let Some(position) = self
                    .columns
                    .iter()
                    .position(|(b, c, _)| b == binding && c == column)
                {
                    Kind::Column(position)
                } else {
                    // A column of an enclosing row is a constant for this subplan.
                    let outer = self
                        .outer
                        .iter()
                        .find(|o| o.binding == *binding && o.column == *column)
                        .ok_or_else(|| {
                            unsupported(format!(
                                "column #{}.{} is not in scope",
                                binding.get(),
                                column.get()
                            ))
                        })?;
                    Kind::Constant(outer.value.clone())
                }
            }
            Expr::Column(ColumnRef::Named { .. }) => {
                return Err(unsupported("named column in an unresolved plan"));
            }
            Expr::Literal(value) => Kind::Constant(Datum::from(value)),
            Expr::Param { index, .. } => {
                match usize::try_from(*index)
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| self.params.get(i))
                {
                    Some(value) => Kind::Constant(value.clone()),
                    None => Kind::Failing(EvalError::Unsupported(format!(
                        "parameter {index} has no value"
                    ))),
                }
            }
            Expr::Call { func, args } => {
                let FunctionRef::Id(id) = func else {
                    return Err(unsupported("function called by name"));
                };
                let def = self
                    .catalog
                    .function_by_id(*id)
                    .ok_or_else(|| unsupported(format!("unknown function {}", id.get())))?;
                let name = def.name.to_string();
                if name == interlaken_ir::security::POLICY_MASK
                    && let [visible, value, masked] = args.as_slice()
                {
                    // The masked value is computed only for the rows it replaces.
                    let kind = Kind::Case {
                        operand: None,
                        branches: vec![(self.compile(visible)?, self.compile(value)?)],
                        otherwise: Some(self.boxed(masked)?),
                    };
                    return Ok(Node::new(kind, ty));
                }
                // SQL's three-valued logic (ADR-0034) is what the connective
                // kernels compute over booleans that may be NULL.
                match (name.as_str(), args.as_slice()) {
                    ("and3", _) => return Ok(Node::new(Kind::And(self.all(args)?), ty)),
                    ("or3", _) => return Ok(Node::new(Kind::Or(self.all(args)?), ty)),
                    ("not3", [arg]) => {
                        return Ok(Node::new(Kind::Unary(UnaryOp::Not, self.boxed(arg)?), ty));
                    }
                    _ => {}
                }
                match Builtin::named(&name) {
                    Some(Builtin::Now) => Kind::Constant(Datum::Timestamptz(self.options.now)),
                    Some(builtin) => Kind::Call(builtin, self.all(args)?),
                    None => Kind::Unknown(name),
                }
            }
            // The inner kernels propagate NULL as SQL's strict operators do.
            Expr::Lift(inner) => return self.compile(inner),
            Expr::Unary { op, expr } => Kind::Unary(*op, self.boxed(expr)?),
            Expr::Binary {
                op: BinaryOp::Like,
                left,
                right,
            } => {
                let pattern = match right.as_ref() {
                    Expr::Literal(Value::Text(p)) => {
                        Pattern::Constant(LikePattern::compile(p.as_str()))
                    }
                    Expr::Cast { expr, .. }
                        if matches!(expr.as_ref(), Expr::Literal(Value::Null)) =>
                    {
                        Pattern::Null
                    }
                    other => Pattern::Dynamic(self.boxed(other)?),
                };
                Kind::Like(self.boxed(left)?, pattern)
            }
            Expr::Binary { op, left, right } => {
                Kind::Binary(*op, self.boxed(left)?, self.boxed(right)?)
            }
            Expr::And(items) => Kind::And(self.all(items)?),
            Expr::Or(items) => Kind::Or(self.all(items)?),
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => Kind::Case {
                operand: operand.as_deref().map(|o| self.boxed(o)).transpose()?,
                branches: branches
                    .iter()
                    .map(|b| Ok((self.compile(&b.when)?, self.compile(&b.then)?)))
                    .collect::<Result<_, ExecError>>()?,
                otherwise: otherwise.as_deref().map(|o| self.boxed(o)).transpose()?,
            },
            Expr::Coalesce(items) => Kind::Coalesce(self.all(items)?),
            Expr::NullIf { left, right } => Kind::NullIf(self.boxed(left)?, self.boxed(right)?),
            Expr::Array(items) => Kind::Array(self.all(items)?),
            Expr::Cast { expr, .. } => Kind::Cast(self.boxed(expr)?),
            Expr::Try { expr, default } => Kind::Try(self.boxed(expr)?, self.boxed(default)?),
            Expr::Subquery { .. } => return Err(unsupported("subquery in an unresolved plan")),
        };
        Ok(Node::new(kind, ty))
    }
}

/// An expression compiled for vectorized evaluation over one input layout.
#[derive(Clone, Debug)]
pub struct CompiledExpr {
    /// The root node.
    root: Node,
    /// Per-evaluation state (the random number generator).
    state: State,
}

impl CompiledExpr {
    /// Compile `expr` for inputs whose columns are `columns`, in order.
    ///
    /// Parameters are bound, references to `outer` columns (enclosing rows
    /// of a correlated subplan) replaced by their values, and constant
    /// `LIKE` patterns compiled here, so each batch only runs kernels.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Unsupported`] if `expr` is unresolved, reads a
    /// column in neither `columns` nor `outer`, or does not type-check.
    pub fn compile(
        expr: &Expr,
        columns: &[(BindingId, ColumnId, Type)],
        outer: &[OuterColumn],
        catalog: &dyn Catalog,
        params: &[Datum],
        options: ExecOptions,
    ) -> Result<Self, ExecError> {
        let scope = columns
            .iter()
            .cloned()
            .chain(outer.iter().map(|o| (o.binding, o.column, o.ty.clone())))
            .collect();
        let root = Compiler {
            columns,
            outer,
            scope,
            catalog,
            params,
            options,
        }
        .compile(expr)?;
        Ok(Self {
            root,
            state: State::new(options.seed),
        })
    }

    /// Return the result type.
    #[must_use]
    pub fn data_type(&self) -> &DataType {
        &self.root.ty
    }

    /// Evaluate the expression on every row of `columns`, which hold `num_rows` rows.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::Eval`] with an error some row raises in the
    /// reference interpreter, if any row raises one.
    pub fn eval(&mut self, columns: &[ArrayRef], num_rows: usize) -> Result<ArrayRef, ExecError> {
        let input = Input::new(columns.iter().cloned().map(Some).collect(), num_rows);
        self.root.eval(&input, &mut self.state)
    }
}
