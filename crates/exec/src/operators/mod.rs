//! Physical operators as pull-based streams of Arrow batches (ADR-0022).

mod aggregate;
mod apply;
mod distinct;
mod filter;
mod join;
mod limit;
mod project;
mod pull;
mod scan;
mod setop;
mod sort;
mod values;

pub(crate) use aggregate::aggregate;
pub(crate) use apply::apply;
pub(crate) use distinct::distinct;
pub(crate) use filter::filter;
pub(crate) use join::join;
pub(crate) use limit::limit;
pub(crate) use project::project;
pub(crate) use scan::table_scan;
pub(crate) use setop::set_op;
pub(crate) use sort::sort;
pub(crate) use values::values;
