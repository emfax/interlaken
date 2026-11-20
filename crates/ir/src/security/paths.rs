//! Entities policies read beyond the principal (ADR-0017, ADR-0032).
//!
//! Partial evaluation knows the principal, but a policy can also read other
//! entities through it (`principal.manager.dept`) or name one directly
//! (`User::"7".dept`). Those entities are not the resource, so lowering
//! can't join them; they must be loaded before partial evaluation. Each
//! [`EntityPath`] names one such entity: a root (the principal or an
//! entity literal) and the attributes followed from it. A provider lists
//! the paths its policies read ([`PolicyProvider::entity_paths`]), and the
//! engine loads each entity on the way through the transaction.
//!
//! [`PolicyProvider::entity_paths`]: super::PolicyProvider::entity_paths

use std::collections::BTreeSet;

use super::eval::EntityStore;
use super::expr::{EntityUid, SecExpr, SecValue};
use crate::names::Ident;

/// An entity a policy reads: its root followed through attributes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityPath {
    /// The root entity: `None` for the principal, or an entity literal.
    pub root: Option<EntityUid>,
    /// The attributes followed from the root, each an entity reference.
    pub attrs: Vec<Ident>,
}

impl EntityPath {
    /// Return the entity this path reaches with `principal` as the principal,
    /// reading attributes from `store`; `None` if an attribute on the way is
    /// absent or not an entity.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::security::{EntityPath, EntityStore, EntityUid, SecValue};
    /// use interlaken_ir::Ident;
    ///
    /// struct Store;
    /// impl EntityStore for Store {
    ///     fn attributes(&self, uid: &EntityUid) -> Option<Vec<(Ident, SecValue)>> {
    ///         (*uid == EntityUid::long("User", 1))
    ///             .then(|| vec![("manager".into(), SecValue::Entity(EntityUid::long("User", 2)))])
    ///     }
    ///     fn ancestors(&self, _: &EntityUid) -> Vec<EntityUid> {
    ///         Vec::new()
    ///     }
    /// }
    /// let path = EntityPath { root: None, attrs: vec!["manager".into()] };
    /// assert_eq!(path.resolve(&EntityUid::long("User", 1), &Store), Some(EntityUid::long("User", 2)));
    /// ```
    #[must_use]
    pub fn resolve(&self, principal: &EntityUid, store: &dyn EntityStore) -> Option<EntityUid> {
        let mut uid = self.root.clone().unwrap_or_else(|| principal.clone());
        for attr in &self.attrs {
            let attributes = store.attributes(&uid)?;
            match attributes.into_iter().find(|(n, _)| n == attr)?.1 {
                SecValue::Entity(next) => uid = next,
                _ => return None,
            }
        }
        Some(uid)
    }
}

/// Return the root and attribute chain of `e` if it is an attribute chain
/// on the principal or an entity literal (`principal.a.b`).
fn chain(e: &SecExpr) -> Option<(Option<EntityUid>, Vec<Ident>)> {
    match e {
        SecExpr::Principal => Some((None, Vec::new())),
        SecExpr::Lit(SecValue::Entity(uid)) => Some((Some(uid.clone()), Vec::new())),
        SecExpr::Attr(base, attr) => {
            let (root, mut attrs) = chain(base)?;
            attrs.push(attr.clone());
            Some((root, attrs))
        }
        _ => None,
    }
}

/// Collect the entities other than the principal that evaluating `e` reads:
/// every entity whose attributes a chain reads (`principal.manager` in
/// `principal.manager.dept`) and every entity whose ancestors `in` tests.
pub fn collect_entity_paths(e: &SecExpr, out: &mut BTreeSet<EntityPath>) {
    let mut add = |root: Option<EntityUid>, attrs: Vec<Ident>| {
        if root.is_some() || !attrs.is_empty() {
            out.insert(EntityPath { root, attrs });
        }
    };
    match e {
        SecExpr::Attr(base, _) | SecExpr::Has(base, _) => {
            if let Some((root, attrs)) = chain(base) {
                add(root, attrs);
            }
        }
        SecExpr::In(member, _) => {
            if let Some((root, attrs)) = chain(member) {
                add(root, attrs);
            }
        }
        _ => {}
    }
    for child in children(e) {
        collect_entity_paths(child, out);
    }
}

/// Return the direct sub-expressions of `e`.
fn children(e: &SecExpr) -> Vec<&SecExpr> {
    match e {
        SecExpr::Attr(a, _)
        | SecExpr::Has(a, _)
        | SecExpr::Is(a, _)
        | SecExpr::Not(a)
        | SecExpr::Like(a, _) => vec![a],
        SecExpr::In(a, b)
        | SecExpr::Contains(a, b)
        | SecExpr::ContainsAll(a, b)
        | SecExpr::ContainsAny(a, b)
        | SecExpr::Cmp(_, a, b)
        | SecExpr::Arith(_, a, b)
        | SecExpr::And(a, b)
        | SecExpr::Or(a, b)
        | SecExpr::Try(a, b) => vec![a, b],
        SecExpr::If(c, a, b) => vec![c, a, b],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::text::parse_sec_expr;

    /// Return the paths `text` reads, rendered as `root.attr...`.
    fn paths(text: &str) -> Vec<String> {
        let mut out = BTreeSet::new();
        collect_entity_paths(&parse_sec_expr(text).unwrap(), &mut out);
        out.into_iter()
            .map(|p| {
                let root = p
                    .root
                    .map_or_else(|| "principal".to_owned(), |u| u.to_string());
                std::iter::once(root)
                    .chain(p.attrs.iter().map(ToString::to_string))
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .collect()
    }

    #[test]
    /// Assert chains through the principal and literals name the entities they read.
    fn paths_of_chains() {
        assert!(paths("(== principal.dept \"eng\")").is_empty());
        assert_eq!(
            paths("(== principal.manager.boss.dept resource.owner.dept)"),
            ["principal.manager", "principal.manager.boss"]
        );
        assert_eq!(
            paths("(and (has principal.manager dept) (in principal.manager (entity Group 1)))"),
            ["principal.manager"]
        );
        assert_eq!(
            paths("(== (attr (entity User 7) dept) \"x\")"),
            ["User::\"7\""]
        );
        assert!(paths("(in principal (entity Group 1))").is_empty());
        assert!(paths("(== resource.owner.dept \"x\")").is_empty());
    }
}
