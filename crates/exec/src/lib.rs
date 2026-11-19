//! The query executor: vectorized, pull-based streams of Arrow batches (ADR-0022).
//!
//! A physical plan is built from optimized IR ([`plan`]) and run as a tree
//! of async streams ([`execute`]): the root pulls, each operator pulls
//! from its inputs as needed, and only the leaves call storage, through the
//! query's transaction. This crate currently provides milestone E1:
//! `TableScan` (with leakproof predicates pushed into the storage scan),
//! `Filter`, `Project` and `Limit`, with expressions evaluated by the
//! reference interpreter's row evaluator until E2's vectorized kernels.

mod columnar;
mod error;
mod exec;
mod kernels;
mod operators;
mod physical;
mod rowwise;

pub use columnar::{arrow_type, build_array, datum_at, field};
pub use error::ExecError;
pub use exec::{BatchStream, ExecContext, collect, execute};
pub use kernels::CompiledExpr;
pub use physical::{BATCH_ROWS, Layout, Op, Physical, PlanOptions, plan, plan_with};
