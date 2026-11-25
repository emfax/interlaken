//! Schema changes as data: the specs DDL operations take, and DDL
//! statements, which IR text parses into and prints from (ADR-0043).

mod specs;
mod statement;

pub use specs::{
    AttributeSpec, ColumnSpec, EntitySpec, ForeignKeySpec, IndexKey, IndexSpec, MembershipSpec,
    ReferentialAction, TableSpec,
};
pub use statement::{DdlStatement, Statement};
