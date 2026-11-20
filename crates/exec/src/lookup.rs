//! Choosing an index nested-loop join (milestone F3, backlog `batched-index-lookup`).
//!
//! A join whose right input is a table scan (under any filters and policy
//! filters) can fetch, for each batch of left rows, only the right rows
//! whose keys match, through the primary key or a secondary index, instead
//! of reading and hashing the whole right table. The planner picks this
//! [`Op::IndexLookup`](crate::Op::IndexLookup) when the join's equality
//! keys pin a leading run of some key's columns, preferring a key they pin
//! completely and uniquely (one storage `get_many` per batch, no scan), then
//! the longest run, then the primary key.

use interlaken_ir::catalog::TableDef;
use interlaken_ir::{ColumnId, ColumnRef, DataType, Expr, Ident, JoinKind, ScalarType};

use crate::physical::{Op, Physical};

/// Which key an index lookup reaches the right table's rows by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupKey {
    /// The primary key: one `get_many` per batch if the lookup pins every
    /// column, otherwise one prefix scan per distinct key in one call.
    Primary,
    /// A secondary index, by name: one prefix scan per distinct key in one
    /// call, then one `get_many` for the rows.
    Index(Ident),
}

/// An index lookup the planner chose for a join.
pub(crate) struct Lookup {
    /// The key reached by.
    pub(crate) key: LookupKey,
    /// The join keys, reordered so the first `prefix` follow the key's columns.
    pub(crate) keys: Vec<(Expr, Expr)>,
    /// How many leading key columns the lookup pins.
    pub(crate) prefix: usize,
    /// Whether the lookup pins the whole primary key, so each key is one point read.
    pub(crate) point: bool,
}

/// Return the table scan under a chain of filters and policy filters, if `node` is one.
pub(crate) fn scan_below_filters(node: &Physical) -> Option<&Physical> {
    match &node.op {
        Op::TableScan { .. } => Some(node),
        Op::Filter { input, .. } | Op::PolicyFilter { input, .. } => scan_below_filters(input),
        _ => None,
    }
}

/// Return whether values of `ty` can be looked up by key: equal keys must
/// mean equal values. Floats (where -0 = 0 and NaN = NaN in SQL) and `jsonb`
/// (which Arrow carries as text) cannot.
fn keyable(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::Scalar(
            ScalarType::Bool
                | ScalarType::Int2
                | ScalarType::Int4
                | ScalarType::Int8
                | ScalarType::Text
                | ScalarType::Bytea
                | ScalarType::Date
                | ScalarType::Timestamptz
                | ScalarType::Uuid
        )
    )
}

/// Return the leading columns of `columns` that `pinned` covers, in order.
fn pinned_prefix(columns: &[ColumnId], pinned: &[Option<ColumnId>]) -> usize {
    columns
        .iter()
        .take_while(|c| pinned.contains(&Some(**c)))
        .count()
}

/// Choose an index lookup for a join of `kind` with hash keys `keys` and right input `right`.
///
/// Returns `None` unless the join is inner, left, semi or anti, the right
/// input is a table scan under filters, and some key of its table has a
/// leading column pinned by a join key equal to one of its columns (of a
/// type keys can represent exactly).
pub(crate) fn choose_lookup(
    kind: JoinKind,
    right: &Physical,
    keys: &[(Expr, Expr)],
) -> Option<Lookup> {
    if !matches!(
        kind,
        JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti
    ) {
        return None;
    }
    let scan = scan_below_filters(right)?;
    let Op::TableScan { table, .. } = &scan.op else {
        return None;
    };
    let binding = scan.layout.first()?.0;
    // The right column each key pins, if it is a plain keyable column of the scan.
    let pinned: Vec<Option<ColumnId>> = keys
        .iter()
        .map(|(_, r)| match r {
            Expr::Column(ColumnRef::Bound { binding: b, column }) if *b == binding => table
                .column_by_id(*column)
                .filter(|c| keyable(&c.ty.data))
                .map(|c| c.id),
            _ => None,
        })
        .collect();
    let candidates = candidates(table);
    // (unique, pinned columns, is primary) — the best wins.
    let (key, columns, unique) = candidates
        .into_iter()
        .map(|(key, columns, unique)| {
            let prefix = pinned_prefix(&columns, &pinned);
            let unique = unique && prefix == columns.len();
            (key, columns, unique, prefix)
        })
        .filter(|(_, _, _, prefix)| *prefix > 0)
        .max_by_key(|(key, _, unique, prefix)| (*unique, *prefix, *key == LookupKey::Primary))
        .map(|(key, columns, unique, prefix)| (key, columns[..prefix].to_vec(), unique))?;
    // The pinning keys first, in key column order, then the rest.
    let mut ordered = Vec::with_capacity(keys.len());
    let mut used = vec![false; keys.len()];
    for column in &columns {
        let i = (0..keys.len()).find(|&i| !used[i] && pinned[i] == Some(*column))?;
        used[i] = true;
        ordered.push(keys[i].clone());
    }
    ordered.extend(
        keys.iter()
            .zip(&used)
            .filter(|(_, u)| !**u)
            .map(|(k, _)| k.clone()),
    );
    Some(Lookup {
        point: unique && key == LookupKey::Primary,
        key,
        keys: ordered,
        prefix: columns.len(),
    })
}

/// Return every key of `table`: (key, its columns, whether it is unique).
fn candidates(table: &TableDef) -> Vec<(LookupKey, Vec<ColumnId>, bool)> {
    let mut out = vec![(LookupKey::Primary, table.primary_key.clone(), true)];
    out.extend(table.indexes.iter().map(|index| {
        (
            LookupKey::Index(index.name.clone()),
            index.columns.iter().map(|c| c.column).collect(),
            index.unique,
        )
    }));
    out
}
