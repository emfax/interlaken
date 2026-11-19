//! The Cedar policy provider (ADR-0020).
//!
//! All Cedar-specific code lives in this module, behind the provider
//! contract, because Cedar's type-aware partial evaluation (TPE) is
//! experimental and its API may change; `cedar-policy` is pinned exactly.
//!
//! - [`schema`] generates the Cedar schema from the catalog's entity
//!   declarations, so policies are validated against real tables.
//! - [`translate`] converts TPE residuals into the security sublanguage,
//!   which the rest of the security core lowers into IR.
//! - [`provider`] implements [`PolicyProvider`](super::PolicyProvider):
//!   partial evaluation with TPE, writes with Cedar's own authorizer.

mod provider;
mod schema;
mod translate;

pub use provider::CedarProvider;
pub use schema::cedar_schema;
