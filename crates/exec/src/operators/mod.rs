//! Physical operators as pull-based streams of Arrow batches (ADR-0022).

mod aggregate;
mod apply;
mod distinct;
mod dml;
mod filter;
mod join;
mod limit;
mod nest;
mod project;
mod pull;
mod scan;
mod setop;
mod sort;
mod unnest;
mod values;
mod write_check;

pub(crate) use aggregate::aggregate;
pub(crate) use apply::apply;
pub(crate) use distinct::distinct;
pub(crate) use dml::{DmlKind, dml};
pub(crate) use filter::filter;
pub(crate) use join::join;
pub(crate) use limit::limit;
pub(crate) use nest::nest;
pub(crate) use project::project;
pub(crate) use scan::table_scan;
pub use scan::{from_stored, stored_type, to_stored};
pub(crate) use setop::set_op;
pub(crate) use sort::sort;
pub(crate) use unnest::unnest;
pub(crate) use values::values;
pub(crate) use write_check::write_check;
