//! The query executor: vectorized, pull-based streams of Arrow batches (ADR-0022).
//!
//! A physical plan is built from optimized IR ([`plan`]) and run as a tree
//! of async streams ([`execute`]): the root pulls, each operator pulls
//! from its inputs as needed, and only the leaves call storage, through the
//! query's transaction. Statements run through a [`Transaction`], which
//! gives them Postgres's atomicity and aborted-transaction semantics.
//!
//! Operators: table scans by primary key range or secondary index (with
//! leakproof predicates pushed into storage), filter, project, limit, hash
//! and nested-loop joins, correlated apply, hash aggregation, distinct,
//! sort and top-N, set operations, values, and insert, update and delete.
//! Expressions are compiled once per operator and evaluated by vectorized
//! kernels with Postgres semantics ([`CompiledExpr`]). `Nest`, `Unnest` and
//! the security operators arrive in milestone E5.

mod access;
mod columnar;
mod error;
mod exec;
mod kernels;
mod operators;
mod physical;
mod rowwise;
mod transaction;

pub use access::{Access, KeyBounds};
pub use columnar::{arrow_type, build_array, datum_at, field};
pub use error::ExecError;
pub use exec::{BatchStream, ExecContext, OuterColumn, collect, execute};
pub use kernels::CompiledExpr;
pub use physical::{Aggregate, BATCH_ROWS, Layout, Op, Physical, PlanOptions, plan, plan_with};
pub use transaction::Transaction;
