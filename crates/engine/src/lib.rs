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

mod attribution;
mod authz;
pub mod catalog;
mod error;
mod feed;
pub mod policy;
pub mod privileges;
mod session;
mod sqlstate;
mod subscription;
mod telemetry;

pub use attribution::Attribution;
pub use authz::{EntityData, LoadedEntities, load_entities, load_principal};
pub use error::EngineError;
pub use feed::{Change, ChangeBatch, ChangeFeed, ChangeFilter, ChangeKind};

/// A commit timestamp (ADR-0040): ordered, the same on every member of a
/// cluster, and written as text with `to_string` and read back with `parse`.
pub use interlaken_storage::Timestamp as CommitTs;
pub use privileges::{Privilege, Privileges};
pub use session::{
    Engine, EngineOptions, QueryResult, QueryStream, ScriptEntry, ScriptReport, Session, Usage,
};
pub use sqlstate::{SQLSTATES, SqlState};
pub use subscription::Subscription;
pub use telemetry::{METRICS, describe_metrics};

/// The README's examples, compiled and run as doc tests so they stay correct.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct ReadmeDoctests;

/// The book's Rust examples (`docs/book`), compiled and run as doc tests so
/// they stay correct. `tests/book_reference.rs` checks every chapter with a
/// Rust example is listed here.
#[cfg(doctest)]
mod book {
    /// Getting started: your first database.
    #[doc = include_str!("../../../docs/book/src/getting-started/first-database.md")]
    struct FirstDatabase;

    /// Getting started: sessions and principals.
    #[doc = include_str!("../../../docs/book/src/getting-started/sessions.md")]
    struct Sessions;

    /// The IR text language: parameters and prepared plans.
    #[doc = include_str!("../../../docs/book/src/ir/parameters.md")]
    struct Parameters;

    /// Transactions: change feeds and read-your-writes.
    #[doc = include_str!("../../../docs/book/src/transactions/change-feeds.md")]
    struct ChangeFeeds;

    /// Data definition: tables.
    #[doc = include_str!("../../../docs/book/src/ddl/tables.md")]
    struct Tables;

    /// Data definition: indexes.
    #[doc = include_str!("../../../docs/book/src/ddl/indexes.md")]
    struct Indexes;

    /// Data definition: entities and memberships.
    #[doc = include_str!("../../../docs/book/src/ddl/entities.md")]
    struct Entities;

    /// Data definition: foreign keys.
    #[doc = include_str!("../../../docs/book/src/ddl/foreign-keys.md")]
    struct ForeignKeys;

    /// Data definition: schema scripts.
    #[doc = include_str!("../../../docs/book/src/ddl/scripts.md")]
    struct Scripts;

    /// Security: privileges.
    #[doc = include_str!("../../../docs/book/src/security/privileges.md")]
    struct Privileges;

    /// Security: changing policies.
    #[doc = include_str!("../../../docs/book/src/security/proposals.md")]
    struct Proposals;

    /// Transactions: isolation.
    #[doc = include_str!("../../../docs/book/src/transactions/isolation.md")]
    struct Isolation;

    /// Transactions: statements and savepoints.
    #[doc = include_str!("../../../docs/book/src/transactions/statements.md")]
    struct Statements;

    /// Transactions: streaming results.
    #[doc = include_str!("../../../docs/book/src/transactions/streaming.md")]
    struct Streaming;

    /// Operations: configuration.
    #[doc = include_str!("../../../docs/book/src/operations/configuration.md")]
    struct Configuration;
}
