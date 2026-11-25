//! The typed relational IR: the engine's single middle end (ADR-0010).
//!
//! Every query language lowers into this IR, which is then resolved,
//! verified, rewritten for security and optimized. This crate currently
//! provides milestones I1 to I4:
//!
//! - The type system: [`ScalarType`], [`DataType`] and [`Type`], with
//!   explicit optionality: a type is `T` or `(option T)` (ADR-0034).
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
//! - [`logic`]: two-valued logic over optional values: turning SQL-style
//!   shorthand into explicit `lift`, `and3` and plain predicates (ADR-0034).
//! - [`verify`]: the verifier every plan passes, which rejects privileged
//!   operators and system-table writes from clients.
//! - [`optimize`]: the rule-based optimizer: simplification, decorrelation,
//!   pushdown under the security barrier, pruning and order (ADR-0014).
//! - [`interp`]: the reference interpreter that defines what a plan means
//!   and serves as the oracle for optimizer and executor tests.
//! - [`security`]: the security core: the policy sublanguage, the provider
//!   contract with an RLS-style provider, and security lowering into
//!   `PolicyFilter` and `WriteCheck` (ADR-0015 to ADR-0018).

pub mod builder;
pub mod catalog;
pub mod ddl;
mod expr;
pub mod interp;
pub mod logic;
mod names;
pub mod optimize;
mod rel;
pub mod resolve;
mod schema;
pub mod security;
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
