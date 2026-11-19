//! Physical operators as pull-based streams of Arrow batches (ADR-0022).

mod filter;
mod limit;
mod project;
mod scan;

pub(crate) use filter::filter;
pub(crate) use limit::limit;
pub(crate) use project::project;
pub(crate) use scan::table_scan;
