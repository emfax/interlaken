//! Public entry points of the text format.

use std::fmt;

use super::error::ParseError;
use super::pretty::render;
use super::sexp::read_one;
use super::{read, write};
use crate::expr::Expr;
use crate::rel::{Plan, Rel};
use crate::types::{DataType, Type};

/// Parse a plan, `(plan REL [:schema-version N])`.
///
/// # Errors
///
/// Returns a [`ParseError`] with the line and column of the first problem
/// if `text` is not exactly one well-formed plan.
///
/// # Examples
///
/// ```
/// use interlaken_ir::text::{parse_plan, print_plan};
///
/// let plan = parse_plan("(plan (filter (get users :as u) (> u.age 18)))")?;
/// assert_eq!(print_plan(&plan), "(plan (filter (get users :as u) (> u.age 18)))");
/// # Ok::<(), interlaken_ir::text::ParseError>(())
/// ```
pub fn parse_plan(text: &str) -> Result<Plan, ParseError> {
    read::plan(&read_one(text)?)
}

/// Parse a relation.
///
/// # Errors
///
/// Returns a [`ParseError`] if `text` is not exactly one well-formed relation.
pub fn parse_rel(text: &str) -> Result<Rel, ParseError> {
    read::rel(&read_one(text)?)
}

/// Parse an expression.
///
/// # Errors
///
/// Returns a [`ParseError`] if `text` is not exactly one well-formed expression.
///
/// # Examples
///
/// ```
/// use interlaken_ir::text::parse_expr;
/// use interlaken_ir::builder::{col, lit};
///
/// assert_eq!(parse_expr("(+ u.age 1)")?, col("u.age") + lit(1i64));
/// # Ok::<(), interlaken_ir::text::ParseError>(())
/// ```
pub fn parse_expr(text: &str) -> Result<Expr, ParseError> {
    read::expr(&read_one(text)?)
}

/// Parse a type, such as `int8` or `(option (array text))`.
///
/// # Errors
///
/// Returns a [`ParseError`] if `text` is not exactly one well-formed type.
pub fn parse_type(text: &str) -> Result<Type, ParseError> {
    read::ty(&read_one(text)?)
}

/// Render a plan in the canonical text format.
#[must_use]
pub fn print_plan(plan: &Plan) -> String {
    render(&write::plan(plan))
}

/// Render a relation in the canonical text format.
#[must_use]
pub fn print_rel(rel: &Rel) -> String {
    render(&write::rel(rel))
}

/// Render an expression in the canonical text format.
#[must_use]
pub fn print_expr(expr: &Expr) -> String {
    render(&write::expr(expr))
}

/// Render a type in the canonical text format.
#[must_use]
pub fn print_type(ty: &Type) -> String {
    render(&write::ty(ty))
}

impl fmt::Display for Plan {
    /// Render the plan in the canonical text format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&print_plan(self))
    }
}

impl fmt::Display for Rel {
    /// Render the relation in the canonical text format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&print_rel(self))
    }
}

impl fmt::Display for Expr {
    /// Render the expression in the canonical text format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&print_expr(self))
    }
}

impl fmt::Display for Type {
    /// Render the type in the canonical text format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&print_type(self))
    }
}

impl fmt::Display for DataType {
    /// Render the data type in the canonical text format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render(&write::data_type(self)))
    }
}
