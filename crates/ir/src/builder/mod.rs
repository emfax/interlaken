//! A dataframe-style Rust builder for IR (ADR-0013).
//!
//! Expressions are plain [`Expr`](crate::Expr) values built with the free
//! functions and methods here (and the arithmetic operators); relations
//! are built by chaining [`RelBuilder`] methods from [`get`] or
//! [`values`]. The builder produces exactly the IR the text format
//! describes, so a query built in Rust equals the same query parsed from
//! text.
//!
//! ```
//! use interlaken_ir::builder::{col, get, lit};
//! use interlaken_ir::text::parse_rel;
//!
//! let built = get("users")
//!     .alias("u")
//!     .filter(col("u.age").gt(lit(18i64)))
//!     .project([col("u.name").alias("name")])
//!     .build();
//! let parsed = parse_rel("(project (filter (get users :as u) (> u.age 18)) (as u.name name))")?;
//! assert_eq!(built, parsed);
//! # Ok::<(), interlaken_ir::text::ParseError>(())
//! ```

mod expr;
mod rel;

pub use expr::{
    Case, agg, and, bound, call, case, case_on, coalesce, col, count_star, exists, lit, null, or,
    param, scalar,
};
pub use rel::{RelBuilder, get, values};
