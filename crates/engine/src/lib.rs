//! The engine: the database's public API, where the IR, the executor and
//! storage meet (ADR-0031).
//!
//! An [`Engine`] opens one database. A [`Session`] runs a principal's
//! statements in transactions: IR text (or built plans) goes in, and rows
//! come out, after resolution and verification against the catalog,
//! security lowering with the active policies, optimization, physical
//! planning and execution, all in one transaction.
//!
//! - [`catalog`]: the IR catalog stored in storage's system tables and the
//!   engine's own, and DDL that changes both (ADR-0013 phase 3).
//! - [`privileges`]: who may change the catalog and policies, and who may
//!   see policy logic in `EXPLAIN`.
//! - [`policy`]: versioned policies changed through verified proposals
//!   (ADR-0020, ADR-0030).
//! - Loading the principal's entity data, and that of the other entities
//!   policies read, from the transaction's snapshot
//!   ([`load_principal`], [`load_entities`], ADR-0017, ADR-0019, ADR-0032).

mod authz;
pub mod catalog;
mod error;
pub mod policy;
pub mod privileges;
mod session;

pub use authz::{EntityData, LoadedEntities, load_entities, load_principal};
pub use error::EngineError;
pub use privileges::{Privilege, Privileges};
pub use session::{Engine, EngineOptions, QueryResult, Session};
