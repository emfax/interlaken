//! The engine layer: where the IR meets storage.
//!
//! This crate currently provides the transactional side of authorization
//! (ADR-0017, ADR-0019): loading the principal's entity data and policies
//! from the transaction's snapshot, so they join its read set and a
//! concurrent revocation or policy change conflicts with transactions that
//! relied on the old state.

mod authz;
mod error;

pub use authz::{
    LoadedEntities, POLICY_TABLE, PolicySet, create_policy_table, load_policies, load_principal,
    store_policies,
};
pub use error::EngineError;
