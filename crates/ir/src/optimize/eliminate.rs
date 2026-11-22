//! Join elimination through foreign keys (ADR-0035 phase 4).
//!
//! An inner join from a child table to its parent on a foreign key finds
//! exactly one parent row for each child row whose referencing columns
//! are present: the foreign key says the row exists, and its key says
//! there is only one. When nothing above the join reads the parent's
//! columns, the join changes nothing and goes. That needs:
//!
//! - **The parent side is a bare `Get`.** A filter there would drop
//!   children whose parent fails it. A `PolicyFilter` would drop children
//!   whose parent the principal can't see, so a governed parent is never
//!   eliminated. Masks wrap the `Get` in a projection, so a masked parent
//!   isn't eliminated either.
//! - **The condition is exactly the foreign key:** an equality between
//!   each referencing column and its referenced column, plain or in the
//!   SQL-style form `(coalesce (lift (= a b)) false)`, and nothing else.
//! - **The referencing columns can't be absent:** plain in their table,
//!   and not padded by an outer join or apply between the child's `Get`
//!   and the join. A row with an absent referencing column has no parent,
//!   and the join would drop it.
//! - **Nothing else reads the parent**, and the join's output isn't
//!   consumed by position (the plan's output, set operations, inserts).
//!
//! Foreign keys hold at the end of every statement, and reads within a
//! statement see the database as of its start plus earlier statements,
//! so a query never sees a row whose parent is missing.

use std::collections::{HashMap, HashSet};

use crate::catalog::Catalog;
use crate::expr::{BinaryOp, Expr};
use crate::names::{BindingId, ColumnId, ColumnRef, TableId, TableRef};
use crate::rel::{ApplyKind, JoinKind, Rel};

/// Remove the joins of `root` that foreign keys make redundant.
///
/// Removing one join can make another redundant, as when a chain of
/// joins reads only its first table, so passes repeat until none removes
/// anything.
pub(crate) fn eliminate(root: &mut Rel, catalog: &dyn Catalog) {
    loop {
        let mut positional = HashSet::new();
        super::prune::positional(root, true, &mut positional);
        let mut counts = HashMap::new();
        count_rel(root, &mut counts);
        let state = State {
            catalog,
            positional,
            counts,
        };
        if !state.rel(root) {
            return;
        }
    }
}

/// Count the references to each binding in `expr`.
fn count_expr(expr: &Expr, counts: &mut HashMap<BindingId, usize>) {
    if let Expr::Column(ColumnRef::Bound { binding, .. }) = expr {
        *counts.entry(*binding).or_default() += 1;
    }
    for child in expr.children() {
        count_expr(child, counts);
    }
}

/// Count the references to each binding in `rel` and below.
fn count_rel(rel: &Rel, counts: &mut HashMap<BindingId, usize>) {
    for expr in rel.exprs() {
        count_expr(expr, counts);
    }
    for input in rel.inputs() {
        count_rel(input, counts);
    }
}

/// Return the table of the `Get` bound to `binding` in `rel`, if its rows
/// reach `rel`'s output unpadded: through no outer side of a join or apply.
fn unpadded_get(rel: &Rel, binding: BindingId) -> Option<TableId> {
    match rel {
        Rel::Get {
            table: TableRef::Id(t),
            binding: Some(b),
            ..
        } if *b == binding => Some(*t),
        Rel::Filter { input, .. }
        | Rel::PolicyFilter { input, .. }
        | Rel::WriteCheck { input, .. }
        | Rel::Sort { input, .. }
        | Rel::Limit { input, .. }
        | Rel::Distinct { input }
        | Rel::Unnest { input, .. } => unpadded_get(input, binding),
        Rel::Join {
            kind, left, right, ..
        } => {
            let from_left = !matches!(kind, JoinKind::Right | JoinKind::Full);
            let from_right = matches!(kind, JoinKind::Inner | JoinKind::Right);
            (if from_left {
                unpadded_get(left, binding)
            } else {
                None
            })
            .or_else(|| {
                if from_right {
                    unpadded_get(right, binding)
                } else {
                    None
                }
            })
        }
        Rel::Apply { kind, outer, inner } => unpadded_get(outer, binding).or_else(|| {
            (*kind == ApplyKind::Inner)
                .then(|| unpadded_get(inner, binding))
                .flatten()
        }),
        _ => None,
    }
}

/// What one elimination pass knows about the whole plan.
struct State<'a> {
    /// The catalog.
    catalog: &'a dyn Catalog,
    /// Bindings whose outputs are consumed by position.
    positional: HashSet<BindingId>,
    /// References to each binding anywhere in the plan.
    counts: HashMap<BindingId, usize>,
}

impl State<'_> {
    /// Eliminate redundant joins in `rel` and below; return whether any went.
    fn rel(&self, rel: &mut Rel) -> bool {
        let mut changed = false;
        for input in rel.inputs_mut() {
            changed |= self.rel(input);
        }
        let Rel::Join {
            kind: JoinKind::Inner,
            left,
            right,
            on,
        } = rel
        else {
            return changed;
        };
        let child = if self.redundant(right, left, on) {
            super::util::take(left)
        } else if self.redundant(left, right, on) {
            super::util::take(right)
        } else {
            return changed;
        };
        *rel = child;
        true
    }

    /// Return `true` if joining `child` to `parent` on `on` changes nothing
    /// anything above reads (see the module documentation).
    fn redundant(&self, parent: &Rel, child: &Rel, on: &Expr) -> bool {
        let Rel::Get {
            table: TableRef::Id(parent_table),
            binding: Some(parent_binding),
            ..
        } = parent
        else {
            return false;
        };
        if self.positional.contains(parent_binding) {
            return false;
        }
        let mut local = HashMap::new();
        count_expr(on, &mut local);
        if self.counts.get(parent_binding) != local.get(parent_binding) {
            return false;
        }
        // Each conjunct pairs a child column with a parent column.
        let mut pairs: Vec<(BindingId, ColumnId, ColumnId)> = Vec::new();
        for conjunct in super::util::conjuncts(on.clone()) {
            let Some((BinaryOp::Eq, a, b)) = crate::logic::comparison(&conjunct) else {
                return false;
            };
            let column = |e: &Expr| match e {
                Expr::Column(ColumnRef::Bound { binding, column }) => Some((*binding, *column)),
                _ => None,
            };
            let (Some(a), Some(b)) = (column(a), column(b)) else {
                return false;
            };
            let ((child_binding, child_column), (_, parent_column)) = match (a, b) {
                (a, b) if b.0 == *parent_binding && a.0 != *parent_binding => (a, b),
                (a, b) if a.0 == *parent_binding && b.0 != *parent_binding => (b, a),
                _ => return false,
            };
            pairs.push((child_binding, child_column, parent_column));
        }
        let Some(&(child_binding, ..)) = pairs.first() else {
            return false;
        };
        if pairs.iter().any(|(b, ..)| *b != child_binding) {
            return false;
        }
        let Some(child_table) = unpadded_get(child, child_binding) else {
            return false;
        };
        let Some(child_def) = self.catalog.table_by_id(child_table) else {
            return false;
        };
        let present = pairs.iter().all(|(_, c, _)| {
            child_def
                .column_by_id(*c)
                .is_some_and(|column| !column.ty.optional)
        });
        if !present {
            return false;
        }
        let wanted: HashSet<(ColumnId, ColumnId)> = pairs.iter().map(|&(_, c, p)| (c, p)).collect();
        self.catalog.foreign_keys_of(child_table).iter().any(|fk| {
            fk.referenced == *parent_table
                && fk.columns.len() == wanted.len()
                && fk
                    .columns
                    .iter()
                    .zip(&fk.referenced_columns)
                    .all(|(c, p)| wanted.contains(&(*c, *p)))
        })
    }
}
