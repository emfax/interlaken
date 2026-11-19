//! Maintaining hierarchy closures by path counting (ADR-0017).
//!
//! The closure holds, for every pair `(descendant, ancestor)` connected by
//! membership edges, the number of distinct paths between them. Adding an
//! edge `m -> g` adds `paths(x, m) * paths(g, y)` for every `x` reaching
//! `m` (including `m`) and every `y` reachable from `g` (including `g`);
//! removing it subtracts the same, and a pair disappears when no path is
//! left. Counting paths, rather than recording reachability, is what makes
//! removal correct when another path still connects a pair. The hierarchy
//! must stay acyclic.

use std::collections::{BTreeMap, BTreeSet};

use super::expr::EntityUid;

/// Adding the edge would make the hierarchy cyclic.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("membership {member} in {group} would create a cycle")]
pub struct CycleError {
    /// The member.
    pub member: EntityUid,
    /// The group.
    pub group: EntityUid,
}

/// A hierarchy's edges and path-counted closure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Closure {
    /// Direct memberships.
    edges: BTreeSet<(EntityUid, EntityUid)>,
    /// Paths between connected pairs `(descendant, ancestor)`, excluding self-pairs.
    paths: BTreeMap<(EntityUid, EntityUid), u64>,
}

impl Closure {
    /// Construct an empty hierarchy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the paths from `from` to `to`, counting `from == to` as one.
    fn count(&self, from: &EntityUid, to: &EntityUid) -> u64 {
        if from == to {
            1
        } else {
            self.paths
                .get(&(from.clone(), to.clone()))
                .copied()
                .unwrap_or(0)
        }
    }

    /// Return `node` and every entity with a path to it, with path counts.
    fn reaching(&self, node: &EntityUid) -> Vec<(EntityUid, u64)> {
        let mut out = vec![(node.clone(), 1)];
        out.extend(
            self.paths
                .iter()
                .filter(|((_, a), _)| a == node)
                .map(|((d, _), n)| (d.clone(), *n)),
        );
        out
    }

    /// Return `node` and every entity it has a path to, with path counts.
    fn reachable(&self, node: &EntityUid) -> Vec<(EntityUid, u64)> {
        let mut out = vec![(node.clone(), 1)];
        out.extend(
            self.paths
                .iter()
                .filter(|((d, _), _)| d == node)
                .map(|((_, a), n)| (a.clone(), *n)),
        );
        out
    }

    /// Add the membership `member in group`; adding an existing edge does nothing.
    ///
    /// # Errors
    ///
    /// Returns [`CycleError`] if `group` is `member` or already a member of it.
    pub fn add_edge(&mut self, member: &EntityUid, group: &EntityUid) -> Result<(), CycleError> {
        if self.count(group, member) > 0 {
            return Err(CycleError {
                member: member.clone(),
                group: group.clone(),
            });
        }
        if !self.edges.insert((member.clone(), group.clone())) {
            return Ok(());
        }
        for (x, a) in self.reaching(member) {
            for (y, b) in self.reachable(group) {
                *self.paths.entry((x.clone(), y)).or_insert(0) += a * b;
            }
        }
        Ok(())
    }

    /// Remove the membership `member in group`; removing a missing edge does nothing.
    pub fn remove_edge(&mut self, member: &EntityUid, group: &EntityUid) {
        if !self.edges.remove(&(member.clone(), group.clone())) {
            return;
        }
        let (reaching, reachable) = (self.reaching(member), self.reachable(group));
        for (x, a) in &reaching {
            for (y, b) in &reachable {
                let key = (x.clone(), y.clone());
                let n = self.paths.get_mut(&key).expect("an added path is counted");
                *n -= a * b;
                if *n == 0 {
                    self.paths.remove(&key);
                }
            }
        }
    }

    /// Return every connected pair `(descendant, ancestor, paths)`.
    pub fn pairs(&self) -> impl Iterator<Item = (&EntityUid, &EntityUid, u64)> {
        self.paths.iter().map(|((d, a), n)| (d, a, *n))
    }

    /// Return the direct memberships.
    pub fn edges(&self) -> impl Iterator<Item = &(EntityUid, EntityUid)> {
        self.edges.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Return node `i` (all one type, so any edge is allowed by type).
    fn node(i: u8) -> EntityUid {
        EntityUid::long("G", i64::from(i))
    }

    /// Return the transitive pairs of `edges` by search.
    fn reachability(edges: &BTreeSet<(EntityUid, EntityUid)>) -> BTreeSet<(EntityUid, EntityUid)> {
        let mut out = BTreeSet::new();
        for (start, _) in edges {
            let mut stack = vec![start.clone()];
            let mut seen = BTreeSet::new();
            while let Some(n) = stack.pop() {
                for (m, g) in edges {
                    if *m == n && seen.insert(g.clone()) {
                        stack.push(g.clone());
                    }
                }
            }
            out.extend(seen.into_iter().map(|g| (start.clone(), g)));
        }
        out
    }

    proptest! {
        #[test]
        /// Assert the closure equals reachability after any sequence of edge changes.
        fn closure_matches_reachability(ops in proptest::collection::vec((any::<bool>(), 0u8..6, 0u8..6), 0..40)) {
            let mut closure = Closure::new();
            for (add, m, g) in ops {
                if add {
                    let _ = closure.add_edge(&node(m), &node(g));
                } else {
                    closure.remove_edge(&node(m), &node(g));
                }
                let edges: BTreeSet<_> = closure.edges().cloned().collect();
                let expected = reachability(&edges);
                let actual: BTreeSet<_> = closure.pairs().map(|(d, a, _)| (d.clone(), a.clone())).collect();
                prop_assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    /// Assert cycles are rejected, including self-membership.
    fn rejects_cycles() {
        let mut closure = Closure::new();
        closure.add_edge(&node(1), &node(2)).unwrap();
        closure.add_edge(&node(2), &node(3)).unwrap();
        assert!(closure.add_edge(&node(3), &node(1)).is_err());
        assert!(closure.add_edge(&node(4), &node(4)).is_err());
    }
}
