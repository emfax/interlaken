//! Transactional MVCC key-value storage layer on fjall.
//!
//! This crate is the bottom of the interlaken stack. It currently provides:
//!
//! - [`encoding`]: the order-preserving tuple encoding used for every key
//!   (ADR-0003) and the versioned key layout (ADR-0002).
//! - [`Engine`]: the fjall database with its `data` and `meta` keyspaces
//!   (ADR-0001, ADR-0002).
//! - [`Timestamp`]: the opaque commit timestamp carried in versioned keys.
//!
//! [`Engine`] is synchronous. Per ADR-0001, fjall is never called from an
//! async task; the storage pool that drives the engine from async code arrives
//! in milestone M2.

pub mod encoding;
mod engine;
mod error;
mod timestamp;

pub use engine::{Engine, FORMAT_VERSION};
pub use error::{Error, Result};
pub use timestamp::Timestamp;
