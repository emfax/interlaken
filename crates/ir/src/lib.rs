//! The typed relational IR: the engine's single middle end (ADR-0010).
//!
//! Every query language lowers into this IR, which is then resolved,
//! verified, rewritten for security and optimized. This crate currently
//! provides milestones I1 and I2:
//!
//! - The type system: [`ScalarType`], [`DataType`] and [`Type`], with
//!   explicit nullability.
//! - Scalar expressions ([`Expr`]) and logical relational operators
//!   ([`Rel`]), in one tree type that carries both the unresolved form
//!   (names) and the resolved form (catalog IDs and bindings); see
//!   [`names`](crate::TableRef) and ADR-0013.
//! - [`builder`]: a dataframe-style Rust builder.
//! - [`text`]: the canonical S-expression text format.
//! - [`catalog`]: the [`Catalog`](catalog::Catalog) trait every IR stage
//!   reads metadata through, with an in-memory implementation and the
//!   built-in functions with their properties (ADR-0011).
//! - [`resolve`]: the shared resolver, from names to resolved, fully typed
//!   IR, lowering subqueries to `Apply`.
//! - [`verify`]: the verifier every plan passes, which rejects privileged
//!   operators and system-table writes from clients.

pub mod builder;
pub mod catalog;
mod expr;
mod names;
mod rel;
pub mod resolve;
mod schema;
pub mod text;
mod types;
mod typing;
mod value;
pub mod verify;

pub use expr::{BinaryOp, CaseBranch, Expr, NamedExpr, SubqueryKind, UnaryOp};
pub use names::{
    BindingId, ColumnId, ColumnRef, FunctionId, FunctionRef, Ident, QualifiedName, SchemaVersion,
    TableId, TableRef, TargetColumn,
};
pub use rel::{
    AggCall, ApplyKind, Assignment, DmlTarget, JoinKind, NamedAgg, Plan, Rel, SetOpKind, SortKey,
};
pub use schema::{OutputColumn, RelSchema};
pub use types::{DataType, Field, ScalarType, Type};
pub use value::Value;
