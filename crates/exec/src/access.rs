//! Choosing a table's access path from the conjuncts of a filter on it (ADR-0028).
//!
//! A conjunct comparing one of the table's columns with an expression that
//! reads none of the table's columns (and cannot raise) constrains that
//! column. Each key — the primary key and every secondary index — takes
//! equalities on a leading run of its columns as a prefix, and at most one
//! lower and one upper bound on the column after it. The best key wins:
//! one that pins a single row (every column of a unique key equal), then the
//! longest prefix, then one with a range, preferring the primary key, which
//! needs no second lookup. Float columns are never keys: -0 and NaN make
//! key equality differ from SQL equality.
//!
//! The filter is kept above the scan regardless, so the access path only
//! has to return a superset of the matching rows.

use std::ops::Bound;

use interlaken_ir::catalog::TableDef;
use interlaken_ir::{BinaryOp, BindingId, ColumnId, ColumnRef, DataType, Expr, Ident, ScalarType};

/// Values for a key: equalities on leading columns, then bounds on the next one.
#[derive(Clone, Debug)]
pub struct KeyBounds {
    /// Values of the leading key columns.
    pub prefix: Vec<Expr>,
    /// The lower bound on the next key column.
    pub lower: Bound<Expr>,
    /// The upper bound on the next key column.
    pub upper: Bound<Expr>,
}

impl KeyBounds {
    /// Return whether the bounds constrain anything.
    fn is_useful(&self) -> bool {
        !self.prefix.is_empty()
            || !matches!(
                (&self.lower, &self.upper),
                (Bound::Unbounded, Bound::Unbounded)
            )
    }
}

/// How a table scan reaches its rows.
#[derive(Clone, Debug)]
pub enum Access {
    /// Every row, in primary key order.
    Full,
    /// The rows under a primary key prefix and range, in primary key order.
    Primary(KeyBounds),
    /// The rows found through a secondary index, in index order.
    Index {
        /// The index name.
        name: Ident,
        /// The index key values.
        bounds: KeyBounds,
    },
}

/// One constraint on a column: `column op value`.
struct Constraint {
    /// The constrained column.
    column: ColumnId,
    /// The comparison, with the column on the left.
    op: BinaryOp,
    /// The value it is compared with.
    value: Expr,
}

/// Return the operator with its operands swapped.
fn flip(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::Le => BinaryOp::Ge,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::Ge => BinaryOp::Le,
        other => other,
    }
}

/// Return the bounds `constraints` give a key of `columns`.
fn bounds(columns: &[ColumnId], constraints: &[Constraint]) -> KeyBounds {
    let find = |column: ColumnId, ops: &[BinaryOp]| {
        constraints
            .iter()
            .find(|c| c.column == column && ops.contains(&c.op))
    };
    let mut prefix = Vec::new();
    for &column in columns {
        match find(column, &[BinaryOp::Eq]) {
            Some(c) => prefix.push(c.value.clone()),
            None => break,
        }
    }
    let (mut lower, mut upper) = (Bound::Unbounded, Bound::Unbounded);
    if let Some(&next) = columns.get(prefix.len()) {
        if let Some(c) = find(next, &[BinaryOp::Gt, BinaryOp::Ge]) {
            lower = if c.op == BinaryOp::Gt {
                Bound::Excluded(c.value.clone())
            } else {
                Bound::Included(c.value.clone())
            };
        }
        if let Some(c) = find(next, &[BinaryOp::Lt, BinaryOp::Le]) {
            upper = if c.op == BinaryOp::Lt {
                Bound::Excluded(c.value.clone())
            } else {
                Bound::Included(c.value.clone())
            };
        }
    }
    KeyBounds {
        prefix,
        lower,
        upper,
    }
}

/// Choose how to scan `table`, bound as `binding`, given the conjuncts of a filter on it.
///
/// `usable` says whether an expression may supply a key value (it must
/// read none of the table's columns and must not raise), and `type_of`
/// gives its type, which must equal the column's for the key to be used.
pub fn choose(
    table: &TableDef,
    binding: BindingId,
    conjuncts: &[Expr],
    usable: impl Fn(&Expr) -> bool,
    type_of: impl Fn(&Expr) -> Option<DataType>,
) -> Access {
    let own_column = |e: &Expr| match e {
        Expr::Column(ColumnRef::Bound { binding: b, column }) if *b == binding => table
            .column_by_id(*column)
            .filter(|c| c.ty.data != DataType::Scalar(ScalarType::Float8)),
        _ => None,
    };
    let constraints: Vec<Constraint> = conjuncts
        .iter()
        .filter_map(|conjunct| {
            let Expr::Binary { op, left, right } = conjunct else {
                return None;
            };
            if !matches!(
                op,
                BinaryOp::Eq | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
            ) {
                return None;
            }
            let (column, op, value) = match (own_column(left), own_column(right)) {
                (Some(c), _) => (c, *op, right.as_ref()),
                (None, Some(c)) => (c, flip(*op), left.as_ref()),
                (None, None) => return None,
            };
            (usable(value) && type_of(value).as_ref() == Some(&column.ty.data)).then(|| {
                Constraint {
                    column: column.id,
                    op,
                    value: value.clone(),
                }
            })
        })
        .collect();
    if constraints.is_empty() {
        return Access::Full;
    }
    // (pins one row, prefix length, has a range, is the primary key)
    let score = |columns: &[ColumnId], unique: bool, bounds: &KeyBounds, primary: bool| {
        let ranged = !matches!(
            (&bounds.lower, &bounds.upper),
            (Bound::Unbounded, Bound::Unbounded)
        );
        (
            unique && bounds.prefix.len() == columns.len(),
            bounds.prefix.len(),
            ranged,
            primary,
        )
    };
    let primary = bounds(&table.primary_key, &constraints);
    let mut best = (
        score(&table.primary_key, true, &primary, true),
        Access::Primary(primary.clone()),
        primary.is_useful(),
    );
    for index in &table.indexes {
        let columns: Vec<ColumnId> = index.columns.iter().map(|c| c.column).collect();
        let b = bounds(&columns, &constraints);
        let s = score(&columns, index.unique, &b, false);
        if b.is_useful() && (!best.2 || s > best.0) {
            best = (
                s,
                Access::Index {
                    name: index.name.clone(),
                    bounds: b,
                },
                true,
            );
        }
    }
    if best.2 { best.1 } else { Access::Full }
}
