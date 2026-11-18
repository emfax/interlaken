//! The shared resolver: names to catalog IDs and bindings, fully typed (ADR-0013).
//!
//! Every front end binds through this resolver, so the rules for name
//! lookup, implicit conversion and subquery lowering exist once. Its output
//! is resolved IR, which the verifier then checks.
//!
//! # Rules
//!
//! - **Names.** An unqualified column name is looked up in the operator's
//!   input, then in enclosing scopes (the outer side of an `Apply`, the
//!   query around a subquery). It is an error if the name matches more
//!   than one visible column of the same scope. A qualified name `q.c`
//!   matches columns of a `Get` aliased (or named) `q`, or of a DML
//!   target. Tables and functions are matched by exact name.
//! - **Bindings.** Each operator that introduces columns gets a fresh
//!   binding, numbered in resolution order (inputs first). Column
//!   references already bound in the input are remapped.
//! - **Implicit conversions.** Operands of an operator, `CASE`, `COALESCE`,
//!   set operation or `VALUES` column are converted to their common type
//!   by integer widening, integer to `float8`, and `date` to `timestamptz`.
//!   A bare integer literal is flexible: it takes the smallest integer
//!   type that holds it, so `int4_col = 5` compares as `int4`. Values
//!   stored by `INSERT` and `UPDATE` may also narrow numerics and convert
//!   to `text`, as in Postgres assignment. An untyped `NULL` takes its
//!   context's type, defaulting to `text`. Every conversion becomes an
//!   explicit cast.
//! - **Functions.** The overload needing the fewest implicit conversions
//!   wins; a tie is an error. Aggregates may only be called in `Aggregate`.
//! - **Subqueries** are lowered to `Apply`. As a top-level conjunct of a
//!   filter, `EXISTS`, `NOT EXISTS`, `IN` and `op ANY` become semi or anti
//!   applies. Elsewhere a scalar subquery becomes a single apply, `EXISTS`
//!   a left apply of one marker row, and `IN`, `ANY` and `ALL` a single
//!   apply over an aggregate that computes the three-valued result.
//!   Subqueries are not supported where evaluating them eagerly would be
//!   wrong (inside `CASE` and `COALESCE` branches, outer join conditions)
//!   or where there is no input to apply to (`VALUES`, `LIMIT`,
//!   `RETURNING`).

mod api;
mod coerce;
mod error;
mod expr;
mod rel;
mod scope;
mod subquery;

pub use api::{Resolved, resolve};
pub use error::ResolveError;
