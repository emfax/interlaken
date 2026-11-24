//! The query executor: vectorized, pull-based streams of Arrow batches (ADR-0022).
//!
//! A physical plan is built from optimized IR ([`plan`]) and run as a tree
//! of async streams ([`execute`]): the root pulls, each operator pulls
//! from its inputs as needed, and only the leaves call storage, through the
//! query's transaction. Statements run through a [`Transaction`], which
//! gives them Postgres's atomicity and aborted-transaction semantics.
//!
//! Operators: table scans by primary key range or secondary index (with
//! leakproof predicates, policy residuals included, pushed into storage),
//! filter, project, limit, hash and nested-loop joins, index lookup joins
//! that probe a keyed table a batch at a time, correlated apply, hash
//! aggregation with incremental accumulators, distinct, sort and top-N, set
//! operations, values, `Nest` and `Unnest`, the security barrier
//! `PolicyFilter` and the per-batch `WriteCheck`, and insert, update and
//! delete, which write a batch per row-layer call. Expressions are compiled
//! once per operator and evaluated by vectorized kernels with Postgres
//! semantics ([`CompiledExpr`]).
//!
//! Blocking operators reserve memory against the query's budget and an
//! optional node-wide pool ([`QueryMemory`], [`MemoryPool`]), failing the
//! query cleanly when either runs out, and yield to the async runtime
//! every few input batches. [`explain`] prints a plan, with
//! policy logic hidden unless asked for, and [`explain_analyze`] runs it
//! and adds per-operator metrics.

mod access;
mod closure;
mod columnar;
mod coop;
mod error;
mod exec;
mod explain;
mod kernels;
mod lookup;
mod memory;
mod metrics;
mod operators;
mod physical;
mod references;
mod rowwise;
mod transaction;
mod usage;

pub use access::{Access, KeyBounds};
pub use closure::ClosurePlan;
pub use columnar::{arrow_type, build_array, datum_at, field};
pub use error::ExecError;
pub use exec::{BatchStream, ExecContext, OuterColumn, collect, execute};
pub use explain::{ExplainOptions, explain, explain_analyze};
pub use kernels::CompiledExpr;
pub use lookup::LookupKey;
pub use memory::{DEFAULT_QUERY_BUDGET, MemoryPool, MemoryScope, QueryMemory};
pub use metrics::{Metrics, NodeMetrics, NodeStats};
pub use operators::{from_stored, stored_type, to_stored};
pub use physical::{Aggregate, BATCH_ROWS, Layout, Op, Physical, PlanOptions, plan, plan_with};
pub use references::{CascadePlans, ReferencePlan, cascade_plans};
pub use transaction::{Statement, Transaction};
pub use usage::UsageCounters;
