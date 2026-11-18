//! Logical relational operators and plans (ADR-0010, ADR-0012).

use crate::expr::{Expr, NamedExpr};
use crate::names::{BindingId, FunctionRef, Ident, SchemaVersion, TableRef, TargetColumn};

/// A complete query: a relation plus, once resolved, the catalog version it was resolved against.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Plan {
    /// The catalog schema version resolved IR is pinned to; `None` when unresolved.
    pub schema_version: Option<SchemaVersion>,
    /// The query.
    pub root: Rel,
}

impl Plan {
    /// Construct an unresolved plan.
    #[must_use]
    pub fn new(root: Rel) -> Self {
        Self {
            schema_version: None,
            root,
        }
    }
}

/// A logical relational operator. Relations are bags of rows.
///
/// Operators that introduce new columns carry a `binding`, which resolved IR
/// always sets (see [`BindingId`]). Operators that pass their input's
/// columns through (`Filter`, `Sort`, `Limit`, `Distinct`, `Join`, `Apply`,
/// `PolicyFilter`, `WriteCheck`) have none.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Rel {
    /// All visible rows of a table at the transaction's snapshot.
    Get {
        /// The table.
        table: TableRef,
        /// The alias column references use as a qualifier.
        alias: Option<Ident>,
        /// The binding of the table's columns.
        binding: Option<BindingId>,
    },
    /// Rows where `predicate` is TRUE.
    Filter {
        /// The input.
        input: Box<Rel>,
        /// The predicate.
        predicate: Expr,
    },
    /// One output row per input row, with computed columns.
    Project {
        /// The input.
        input: Box<Rel>,
        /// The output columns.
        exprs: Vec<NamedExpr>,
        /// The binding of the output columns.
        binding: Option<BindingId>,
    },
    /// A join; the output has the left columns then the right columns
    /// (only the left for semi and anti joins).
    Join {
        /// The join kind.
        kind: JoinKind,
        /// The left input.
        left: Box<Rel>,
        /// The right input.
        right: Box<Rel>,
        /// The join condition; `true` for a cross join.
        on: Expr,
    },
    /// A dependent join: `inner` is evaluated for each `outer` row and may
    /// reference its columns. Correlated subqueries and LATERAL lower to this.
    Apply {
        /// The apply kind.
        kind: ApplyKind,
        /// The outer input.
        outer: Box<Rel>,
        /// The inner input, which may reference `outer`'s columns.
        inner: Box<Rel>,
    },
    /// Grouping and aggregation; the output has the keys then the aggregates.
    ///
    /// With no keys, the output is exactly one row.
    Aggregate {
        /// The input.
        input: Box<Rel>,
        /// The grouping keys.
        keys: Vec<NamedExpr>,
        /// The aggregates.
        aggs: Vec<NamedAgg>,
        /// The binding of the output columns.
        binding: Option<BindingId>,
    },
    /// Remove duplicate rows.
    Distinct {
        /// The input.
        input: Box<Rel>,
    },
    /// Order rows.
    Sort {
        /// The input.
        input: Box<Rel>,
        /// The sort keys, most significant first.
        keys: Vec<SortKey>,
    },
    /// Skip `offset` rows, then return at most `limit` rows.
    Limit {
        /// The input.
        input: Box<Rel>,
        /// The maximum number of rows; unlimited when `None`.
        limit: Option<Expr>,
        /// The number of rows to skip; zero when `None`.
        offset: Option<Expr>,
    },
    /// `UNION`, `INTERSECT` or `EXCEPT`, with or without `ALL`. Output columns take the left input's names.
    SetOp {
        /// The operation.
        op: SetOpKind,
        /// `true` for bag semantics (`ALL`), `false` to remove duplicates.
        all: bool,
        /// The left input.
        left: Box<Rel>,
        /// The right input.
        right: Box<Rel>,
        /// The binding of the output columns.
        binding: Option<BindingId>,
    },
    /// A literal relation.
    Values {
        /// The column names.
        columns: Vec<Ident>,
        /// The rows; each has one expression per column.
        rows: Vec<Vec<Expr>>,
        /// The binding of the columns.
        binding: Option<BindingId>,
    },
    /// Group rows by `keys` and collect each group's `fields` into an
    /// `array(record(...))` column named `name`, for nested results (ADR-0012).
    ///
    /// The output has the keys then the nested column.
    Nest {
        /// The input.
        input: Box<Rel>,
        /// The grouping keys.
        keys: Vec<NamedExpr>,
        /// The record fields, evaluated per input row.
        fields: Vec<NamedExpr>,
        /// The name of the nested column.
        name: Ident,
        /// The binding of the output columns.
        binding: Option<BindingId>,
    },
    /// Expand an array into rows: one output row per element, with the input
    /// columns followed by the element's columns (a record's fields, or one
    /// column for a scalar element).
    Unnest {
        /// The input.
        input: Box<Rel>,
        /// The array expression, evaluated per input row.
        expr: Expr,
        /// The names of the element's columns.
        columns: Vec<Ident>,
        /// The binding of the element's columns.
        binding: Option<BindingId>,
    },
    /// Insert the input's rows into a table.
    Insert {
        /// The target table.
        target: DmlTarget,
        /// The target columns, matching the input's columns in order.
        columns: Vec<TargetColumn>,
        /// The rows to insert.
        input: Box<Rel>,
        /// `RETURNING` expressions over the inserted row.
        returning: Vec<NamedExpr>,
        /// The binding of the `RETURNING` columns.
        binding: Option<BindingId>,
    },
    /// Update the target rows produced by the input, which carries their keys.
    Update {
        /// The target table.
        target: DmlTarget,
        /// The rows to update.
        input: Box<Rel>,
        /// The new column values, as expressions over the input.
        assignments: Vec<Assignment>,
        /// `RETURNING` expressions over the updated row.
        returning: Vec<NamedExpr>,
        /// The binding of the `RETURNING` columns.
        binding: Option<BindingId>,
    },
    /// Delete the target rows produced by the input, which carries their keys.
    Delete {
        /// The target table.
        target: DmlTarget,
        /// The rows to delete.
        input: Box<Rel>,
        /// `RETURNING` expressions over the deleted row.
        returning: Vec<NamedExpr>,
        /// The binding of the `RETURNING` columns.
        binding: Option<BindingId>,
    },
    /// **Privileged.** A security barrier keeping rows where `residual` is
    /// TRUE (ADR-0018). Only the engine's security lowering may create it.
    PolicyFilter {
        /// The input.
        input: Box<Rel>,
        /// The policy residual.
        residual: Expr,
    },
    /// **Privileged.** Per-row authorization of DML input (ADR-0018). Only
    /// the engine's security lowering may create it.
    WriteCheck {
        /// The input.
        input: Box<Rel>,
        /// The check every row must pass.
        check: Expr,
    },
}

/// The kind of a [`Rel::Join`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JoinKind {
    /// Inner join.
    Inner,
    /// Left outer join.
    Left,
    /// Right outer join.
    Right,
    /// Full outer join.
    Full,
    /// Left rows with at least one match.
    Semi,
    /// Left rows with no match.
    Anti,
}

/// The kind of a [`Rel::Apply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApplyKind {
    /// Outer rows paired with each inner row (LATERAL, `IN` lowered as a join).
    Inner,
    /// Like `Inner`, but outer rows with no inner rows are kept, padded with NULLs.
    Left,
    /// Outer rows for which the inner relation is non-empty (`EXISTS`).
    Semi,
    /// Outer rows for which the inner relation is empty (`NOT EXISTS`).
    Anti,
    /// Like `Left`, but more than one inner row is an error (scalar subqueries).
    Single,
}

/// The operation of a [`Rel::SetOp`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SetOpKind {
    /// `UNION`.
    Union,
    /// `INTERSECT`.
    Intersect,
    /// `EXCEPT`.
    Except,
}

/// One sort key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SortKey {
    /// The expression sorted on.
    pub expr: Expr,
    /// `true` for descending order.
    pub descending: bool,
    /// `true` if NULLs sort before other values.
    pub nulls_first: bool,
}

impl SortKey {
    /// Construct an ascending key; NULLs last, as in Postgres.
    #[must_use]
    pub fn asc(expr: Expr) -> Self {
        Self {
            expr,
            descending: false,
            nulls_first: false,
        }
    }

    /// Construct a descending key; NULLs first, as in Postgres.
    #[must_use]
    pub fn desc(expr: Expr) -> Self {
        Self {
            expr,
            descending: true,
            nulls_first: true,
        }
    }

    /// Return a copy with the given NULL placement.
    #[must_use]
    pub fn nulls_first(mut self, first: bool) -> Self {
        self.nulls_first = first;
        self
    }

    /// Return `true` if NULL placement is the Postgres default for the direction.
    #[must_use]
    pub const fn has_default_nulls(&self) -> bool {
        self.nulls_first == self.descending
    }
}

/// An aggregate function call.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AggCall {
    /// The aggregate function.
    pub func: FunctionRef,
    /// The arguments; empty for `count(*)`.
    pub args: Vec<Expr>,
    /// `true` to aggregate distinct argument values only.
    pub distinct: bool,
    /// Rows for which this is not TRUE are skipped (`FILTER (WHERE ...)`).
    pub filter: Option<Expr>,
}

/// An aggregate call with an optional output column name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NamedAgg {
    /// The aggregate call.
    pub agg: AggCall,
    /// The output column name, if given.
    pub name: Option<Ident>,
}

impl From<AggCall> for NamedAgg {
    /// Wrap an aggregate call without an output name.
    fn from(agg: AggCall) -> Self {
        Self { agg, name: None }
    }
}

/// The target table of a DML operator.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DmlTarget {
    /// The table.
    pub table: TableRef,
    /// The alias `RETURNING` expressions use for the target row.
    pub alias: Option<Ident>,
    /// The binding of the target row (new values for insert and update,
    /// old values for delete), used by `RETURNING`.
    pub binding: Option<BindingId>,
}

impl DmlTarget {
    /// Construct a target with no alias or binding.
    #[must_use]
    pub fn new(table: impl Into<TableRef>) -> Self {
        Self {
            table: table.into(),
            alias: None,
            binding: None,
        }
    }
}

/// One `SET column = value` of an update.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Assignment {
    /// The column assigned.
    pub column: TargetColumn,
    /// The new value, over the update's input.
    pub value: Expr,
}

impl Rel {
    /// Return the input relations, in order.
    ///
    /// Relations inside subquery expressions are not included.
    #[must_use]
    pub fn inputs(&self) -> Vec<&Rel> {
        match self {
            Self::Get { .. } | Self::Values { .. } => Vec::new(),
            Self::Filter { input, .. }
            | Self::Project { input, .. }
            | Self::Aggregate { input, .. }
            | Self::Distinct { input }
            | Self::Sort { input, .. }
            | Self::Limit { input, .. }
            | Self::Nest { input, .. }
            | Self::Unnest { input, .. }
            | Self::Insert { input, .. }
            | Self::Update { input, .. }
            | Self::Delete { input, .. }
            | Self::PolicyFilter { input, .. }
            | Self::WriteCheck { input, .. } => vec![input],
            Self::Join { left, right, .. } | Self::SetOp { left, right, .. } => vec![left, right],
            Self::Apply { outer, inner, .. } => vec![outer, inner],
        }
    }

    /// Return the input relations mutably, in the same order as [`Rel::inputs`].
    pub fn inputs_mut(&mut self) -> Vec<&mut Rel> {
        match self {
            Self::Get { .. } | Self::Values { .. } => Vec::new(),
            Self::Filter { input, .. }
            | Self::Project { input, .. }
            | Self::Aggregate { input, .. }
            | Self::Distinct { input }
            | Self::Sort { input, .. }
            | Self::Limit { input, .. }
            | Self::Nest { input, .. }
            | Self::Unnest { input, .. }
            | Self::Insert { input, .. }
            | Self::Update { input, .. }
            | Self::Delete { input, .. }
            | Self::PolicyFilter { input, .. }
            | Self::WriteCheck { input, .. } => vec![input],
            Self::Join { left, right, .. } | Self::SetOp { left, right, .. } => vec![left, right],
            Self::Apply { outer, inner, .. } => vec![outer, inner],
        }
    }

    /// Return the expressions this operator evaluates directly (not its inputs').
    #[must_use]
    pub fn exprs(&self) -> Vec<&Expr> {
        match self {
            Self::Get { .. } | Self::Distinct { .. } | Self::Apply { .. } | Self::SetOp { .. } => {
                Vec::new()
            }
            Self::Filter { predicate: e, .. }
            | Self::Join { on: e, .. }
            | Self::Unnest { expr: e, .. }
            | Self::PolicyFilter { residual: e, .. }
            | Self::WriteCheck { check: e, .. } => vec![e],
            Self::Project { exprs, .. } => exprs.iter().map(|n| &n.expr).collect(),
            Self::Aggregate { keys, aggs, .. } => keys
                .iter()
                .map(|n| &n.expr)
                .chain(
                    aggs.iter()
                        .flat_map(|a| a.agg.args.iter().chain(a.agg.filter.iter())),
                )
                .collect(),
            Self::Sort { keys, .. } => keys.iter().map(|k| &k.expr).collect(),
            Self::Limit { limit, offset, .. } => limit.iter().chain(offset.iter()).collect(),
            Self::Values { rows, .. } => rows.iter().flatten().collect(),
            Self::Nest { keys, fields, .. } => {
                keys.iter().chain(fields.iter()).map(|n| &n.expr).collect()
            }
            Self::Insert { returning, .. } | Self::Delete { returning, .. } => {
                returning.iter().map(|n| &n.expr).collect()
            }
            Self::Update {
                assignments,
                returning,
                ..
            } => assignments
                .iter()
                .map(|a| &a.value)
                .chain(returning.iter().map(|n| &n.expr))
                .collect(),
        }
    }

    /// Return `true` for operators only the engine's own passes may create.
    #[must_use]
    pub const fn is_privileged(&self) -> bool {
        matches!(self, Self::PolicyFilter { .. } | Self::WriteCheck { .. })
    }
}
