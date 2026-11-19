//! The security core: policies compiled into the IR (ADR-0015 to ADR-0019).
//!
//! Policies come from a [`PolicyProvider`] and are written in a small
//! sublanguage ([`SecExpr`]). Before a query runs, [`secure`] asks the
//! provider for each governed table's residual (the policies partially
//! evaluated for the principal and context), lowers it into IR, and adds
//! `PolicyFilter` barriers above scans and `WriteCheck`s on writes. The
//! optimizer then moves only leakproof predicates below the barriers.
//!
//! [`RlsProvider`] is a Postgres-RLS-style provider whose policies are
//! sublanguage predicates; Cedar follows in I5. [`Closure`] maintains
//! hierarchy closure tables, and [`DatabaseEntities`] reads entity data
//! from the reference interpreter's database.

mod cache;
mod cedar;
mod closure;
mod error;
mod eval;
mod expr;
mod impact;
mod lower;
mod plan;
mod provider;
mod store;
mod text;

pub use cache::{CacheStats, CachedProvider, ResidualCache, VOLATILE_CONTEXT_FIELDS};
pub use cedar::{CedarProvider, cedar_schema};
pub use closure::{Closure, CycleError};
pub use error::SecurityError;
pub use eval::{EntityStore, EvalFailure, ResourceData};
pub use expr::{Action, ArithOp, CmpOp, Effect, EntityKey, EntityUid, Policy, SecExpr, SecValue};
pub use impact::{Change, ImpactReport, forbid_hazard, impact_analysis};
pub use plan::secure;
pub use provider::{
    CombiningRule, Decision, Dependencies, PartialRequest, PolicyProvider, Request, Residual,
    RlsProvider, SecurityContext, StandardContext,
};
pub use store::DatabaseEntities;
pub use text::{parse_policies, parse_sec_expr, print_sec_expr};
