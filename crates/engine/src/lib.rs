//! The engine layer: where the IR meets storage.
//!
//! This crate currently provides the transactional side of authorization:
//! loading the principal's entity data from the transaction's snapshot
//! (ADR-0017, ADR-0019), and [`policy`]: versioned policies changed through
//! verified proposals (ADR-0020, ADR-0030).

mod authz;
mod error;
pub mod policy;

pub use authz::{LoadedEntities, load_principal};
pub use error::EngineError;
