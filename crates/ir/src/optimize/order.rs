//! Phase 5: limit pushdown and sort order as a physical property (ADR-0014).
//!
//! Each relation's output order is derived from the plan (the Calcite-style
//! "trait"): a sort defines it, and filters, limits and security operators
//! preserve it. A sort whose input is already in the required order is
//! removed, as is a sort directly below another sort. A limit moves below a
//! projection, so fewer rows are projected.

use super::util::{take, volatile};
use crate::catalog::Catalog;
use crate::rel::{Rel, SortKey};

/// Return the order `rel`'s output is known to have (empty when none).
pub(crate) fn ordering(rel: &Rel) -> Vec<SortKey> {
    match rel {
        Rel::Sort { keys, .. } => keys.clone(),
        Rel::Filter { input, .. }
        | Rel::PolicyFilter { input, .. }
        | Rel::WriteCheck { input, .. }
        | Rel::Limit { input, .. } => ordering(input),
        _ => Vec::new(),
    }
}

/// Apply the order rules to `rel` and below.
pub(crate) fn order(rel: &mut Rel, catalog: &dyn Catalog) {
    for input in rel.inputs_mut() {
        order(input, catalog);
    }
    loop {
        let replacement = match rel {
            Rel::Sort { input, keys } => match input.as_mut() {
                Rel::Sort { input: inner, .. } => Some(Rel::Sort {
                    input: Box::new(take(inner)),
                    keys: std::mem::take(keys),
                }),
                other if ordering(other).starts_with(keys) => Some(take(other)),
                _ => None,
            },
            Rel::Limit {
                input,
                limit,
                offset,
            } => match input.as_mut() {
                Rel::Project {
                    input: inner,
                    exprs,
                    binding,
                } if !exprs.iter().any(|e| volatile(&e.expr, catalog)) => Some(Rel::Project {
                    input: Box::new(Rel::Limit {
                        input: Box::new(take(inner)),
                        limit: limit.take(),
                        offset: offset.take(),
                    }),
                    exprs: std::mem::take(exprs),
                    binding: *binding,
                }),
                _ => None,
            },
            _ => None,
        };
        match replacement {
            Some(r) => {
                *rel = r;
                for input in rel.inputs_mut() {
                    order(input, catalog);
                }
            }
            None => return,
        }
    }
}
