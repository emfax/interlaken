//! The marker on the operators security lowering adds (ADR-0025).
//!
//! Every table lowering reads in a policy subquery is aliased, and every
//! literal relation it builds has its columns named, with
//! [`POLICY_ALIAS_PREFIX`] and a number. Client plans may not use the
//! prefix (the verifier rejects it in untrusted plans), so the marker
//! survives optimization and planning and tells `EXPLAIN` exactly which
//! subplans to hide from principals who may not read policies.

use crate::names::Ident;

/// The prefix of the aliases and column names security lowering generates.
pub const POLICY_ALIAS_PREFIX: &str = "sec$";

/// Return whether `name` is an alias or column name security lowering generated.
///
/// # Examples
///
/// ```
/// use interlaken_ir::Ident;
/// use interlaken_ir::security::is_policy_alias;
///
/// assert!(is_policy_alias(&Ident::new("sec$3")));
/// assert!(!is_policy_alias(&Ident::new("docs")));
/// ```
#[must_use]
pub fn is_policy_alias(name: &Ident) -> bool {
    name.as_str().starts_with(POLICY_ALIAS_PREFIX)
}
