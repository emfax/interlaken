//! The typed relational IR: the engine's single middle end (ADR-0010).
//!
//! Every query language lowers into this IR, which is then resolved,
//! verified, rewritten for security and optimized. This crate currently
//! provides milestone I1:
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
//!   reads metadata through, with an in-memory implementation.

pub mod builder;
pub mod catalog;
mod expr;
mod names;
mod rel;
pub mod text;
mod types;
mod value;

pub use expr::{BinaryOp, CaseBranch, Expr, NamedExpr, SubqueryKind, UnaryOp};
pub use names::{
    BindingId, ColumnId, ColumnRef, FunctionId, FunctionRef, Ident, QualifiedName, SchemaVersion,
    TableId, TableRef, TargetColumn,
};
pub use rel::{
    AggCall, ApplyKind, Assignment, DmlTarget, JoinKind, NamedAgg, Plan, Rel, SetOpKind, SortKey,
};
pub use types::{DataType, Field, ScalarType, Type};
pub use value::Value;
