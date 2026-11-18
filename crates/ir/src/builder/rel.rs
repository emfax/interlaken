//! Building relations.

use crate::expr::{Expr, NamedExpr};
use crate::names::{BindingId, Ident, TableRef, TargetColumn};
use crate::rel::{
    ApplyKind, Assignment, DmlTarget, JoinKind, NamedAgg, Plan, Rel, SetOpKind, SortKey,
};

/// Start a relation with all rows of `table`.
pub fn get(table: impl Into<TableRef>) -> RelBuilder {
    RelBuilder(Rel::Get {
        table: table.into(),
        alias: None,
        binding: None,
    })
}

/// Start a relation from literal rows.
pub fn values<C, R>(columns: C, rows: R) -> RelBuilder
where
    C: IntoIterator,
    C::Item: Into<Ident>,
    R: IntoIterator,
    R::Item: IntoIterator<Item = Expr>,
{
    RelBuilder(Rel::Values {
        columns: columns.into_iter().map(Into::into).collect(),
        rows: rows
            .into_iter()
            .map(|row| row.into_iter().collect())
            .collect(),
        binding: None,
    })
}

/// Collect output items.
fn items<I>(items: I) -> Vec<NamedExpr>
where
    I: IntoIterator,
    I::Item: Into<NamedExpr>,
{
    items.into_iter().map(Into::into).collect()
}

/// A relation under construction. Each method wraps the current relation
/// in a new operator, so calls read in data-flow order.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
pub struct RelBuilder(Rel);

impl RelBuilder {
    /// Wrap an existing relation.
    pub fn from_rel(rel: Rel) -> Self {
        Self(rel)
    }

    /// Finish, returning the relation.
    #[must_use]
    pub fn build(self) -> Rel {
        self.0
    }

    /// Finish, returning an unresolved plan.
    #[must_use]
    pub fn plan(self) -> Plan {
        Plan::new(self.0)
    }

    /// Set the alias of a `Get`, or of a DML operator's target row.
    ///
    /// # Panics
    ///
    /// Panics if the current operator is not `Get`, `Insert`, `Update` or `Delete`.
    pub fn alias(mut self, name: impl Into<Ident>) -> Self {
        let name = Some(name.into());
        match &mut self.0 {
            Rel::Get { alias, .. } => *alias = name,
            Rel::Insert { target, .. }
            | Rel::Update { target, .. }
            | Rel::Delete { target, .. } => target.alias = name,
            other => panic!("alias applies to get and DML operators, not {other:?}"),
        }
        self
    }

    /// Set the binding of the current operator's output columns.
    ///
    /// # Panics
    ///
    /// Panics if the current operator passes its input's columns through
    /// and so has no binding (`Filter`, `Join`, `Sort` and the like).
    pub fn bind(mut self, id: u32) -> Self {
        let id = Some(BindingId::new(id));
        match &mut self.0 {
            Rel::Get { binding, .. }
            | Rel::Project { binding, .. }
            | Rel::Aggregate { binding, .. }
            | Rel::SetOp { binding, .. }
            | Rel::Values { binding, .. }
            | Rel::Nest { binding, .. }
            | Rel::Unnest { binding, .. }
            | Rel::Insert { binding, .. }
            | Rel::Update { binding, .. }
            | Rel::Delete { binding, .. } => *binding = id,
            other => panic!("{other:?} has no binding"),
        }
        self
    }

    /// Set the binding of a DML operator's target row.
    ///
    /// # Panics
    ///
    /// Panics if the current operator is not `Insert`, `Update` or `Delete`.
    pub fn bind_target(mut self, id: u32) -> Self {
        self.target_mut().binding = Some(BindingId::new(id));
        self
    }

    /// Set a DML operator's `RETURNING` items.
    ///
    /// # Panics
    ///
    /// Panics if the current operator is not `Insert`, `Update` or `Delete`.
    pub fn returning<I>(mut self, exprs: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<NamedExpr>,
    {
        match &mut self.0 {
            Rel::Insert { returning, .. }
            | Rel::Update { returning, .. }
            | Rel::Delete { returning, .. } => *returning = items(exprs),
            other => panic!("returning applies to DML operators, not {other:?}"),
        }
        self
    }

    /// Return the current DML operator's target.
    fn target_mut(&mut self) -> &mut DmlTarget {
        match &mut self.0 {
            Rel::Insert { target, .. }
            | Rel::Update { target, .. }
            | Rel::Delete { target, .. } => target,
            other => panic!("expected a DML operator, not {other:?}"),
        }
    }

    /// Return the current relation, boxed, for wrapping.
    fn input(self) -> Box<Rel> {
        Box::new(self.0)
    }

    /// Keep rows where `predicate` is TRUE.
    pub fn filter(self, predicate: Expr) -> Self {
        Self(Rel::Filter {
            input: self.input(),
            predicate,
        })
    }

    /// Compute output columns.
    pub fn project<I>(self, exprs: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<NamedExpr>,
    {
        Self(Rel::Project {
            input: self.input(),
            exprs: items(exprs),
            binding: None,
        })
    }

    /// Join with `right` on `on`.
    pub fn join(self, kind: JoinKind, right: impl Into<Rel>, on: Expr) -> Self {
        Self(Rel::Join {
            kind,
            left: self.input(),
            right: Box::new(right.into()),
            on,
        })
    }

    /// Evaluate `inner` for each row, as a dependent join.
    pub fn apply(self, kind: ApplyKind, inner: impl Into<Rel>) -> Self {
        Self(Rel::Apply {
            kind,
            outer: self.input(),
            inner: Box::new(inner.into()),
        })
    }

    /// Group by `keys` and compute `aggs`.
    pub fn aggregate<K, A>(self, keys: K, aggs: A) -> Self
    where
        K: IntoIterator,
        K::Item: Into<NamedExpr>,
        A: IntoIterator,
        A::Item: Into<NamedAgg>,
    {
        Self(Rel::Aggregate {
            input: self.input(),
            keys: items(keys),
            aggs: aggs.into_iter().map(Into::into).collect(),
            binding: None,
        })
    }

    /// Remove duplicate rows.
    pub fn distinct(self) -> Self {
        Self(Rel::Distinct {
            input: self.input(),
        })
    }

    /// Order rows by `keys`.
    pub fn sort(self, keys: impl IntoIterator<Item = SortKey>) -> Self {
        Self(Rel::Sort {
            input: self.input(),
            keys: keys.into_iter().collect(),
        })
    }

    /// Return at most `count` rows.
    pub fn limit(self, count: Expr) -> Self {
        self.limit_offset(Some(count), None)
    }

    /// Skip `offset` rows, then return at most `limit` rows.
    pub fn limit_offset(self, limit: Option<Expr>, offset: Option<Expr>) -> Self {
        Self(Rel::Limit {
            input: self.input(),
            limit,
            offset,
        })
    }

    /// Combine with `right` by a set operation.
    pub fn set_op(self, op: SetOpKind, all: bool, right: impl Into<Rel>) -> Self {
        Self(Rel::SetOp {
            op,
            all,
            left: self.input(),
            right: Box::new(right.into()),
            binding: None,
        })
    }

    /// `UNION` (`all` keeps duplicates).
    pub fn union(self, right: impl Into<Rel>, all: bool) -> Self {
        self.set_op(SetOpKind::Union, all, right)
    }

    /// Group by `keys` and collect each group's `fields` into an array column named `name`.
    pub fn nest<K, F>(self, keys: K, fields: F, name: impl Into<Ident>) -> Self
    where
        K: IntoIterator,
        K::Item: Into<NamedExpr>,
        F: IntoIterator,
        F::Item: Into<NamedExpr>,
    {
        Self(Rel::Nest {
            input: self.input(),
            keys: items(keys),
            fields: items(fields),
            name: name.into(),
            binding: None,
        })
    }

    /// Expand the array `expr` into rows with the given element column names.
    pub fn unnest<C>(self, expr: Expr, columns: C) -> Self
    where
        C: IntoIterator,
        C::Item: Into<Ident>,
    {
        Self(Rel::Unnest {
            input: self.input(),
            expr,
            columns: columns.into_iter().map(Into::into).collect(),
            binding: None,
        })
    }

    /// Insert the current rows into `table`'s `columns`.
    pub fn insert_into<C>(self, table: impl Into<TableRef>, columns: C) -> Self
    where
        C: IntoIterator,
        C::Item: Into<TargetColumn>,
    {
        Self(Rel::Insert {
            target: DmlTarget::new(table),
            columns: columns.into_iter().map(Into::into).collect(),
            input: self.input(),
            returning: Vec::new(),
            binding: None,
        })
    }

    /// Update the current rows of `table` with `assignments` (column, value).
    pub fn update<C, A>(self, table: impl Into<TableRef>, assignments: A) -> Self
    where
        C: Into<TargetColumn>,
        A: IntoIterator<Item = (C, Expr)>,
    {
        Self(Rel::Update {
            target: DmlTarget::new(table),
            input: self.input(),
            assignments: assignments
                .into_iter()
                .map(|(column, value)| Assignment {
                    column: column.into(),
                    value,
                })
                .collect(),
            returning: Vec::new(),
            binding: None,
        })
    }

    /// Delete the current rows from `table`.
    pub fn delete_from(self, table: impl Into<TableRef>) -> Self {
        Self(Rel::Delete {
            target: DmlTarget::new(table),
            input: self.input(),
            returning: Vec::new(),
            binding: None,
        })
    }

    /// Add a security barrier. Privileged: the verifier rejects it from clients.
    pub fn policy_filter(self, residual: Expr) -> Self {
        Self(Rel::PolicyFilter {
            input: self.input(),
            residual,
        })
    }

    /// Add a per-row write check. Privileged: the verifier rejects it from clients.
    pub fn write_check(self, check: Expr) -> Self {
        Self(Rel::WriteCheck {
            input: self.input(),
            check,
        })
    }
}
