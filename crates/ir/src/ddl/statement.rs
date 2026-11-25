//! DDL statements, one per schema-changing operation (ADR-0043).

use super::specs::{ColumnSpec, EntitySpec, ForeignKeySpec, IndexSpec, MembershipSpec, TableSpec};
use crate::rel::Plan;
use crate::security::EntityUid;
use crate::types::DataType;

/// One statement of IR text: a plan, or a schema change.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    /// A query or DML statement, `(plan ...)`.
    Plan(Plan),
    /// A schema change, such as `(create-table ...)`.
    Ddl(DdlStatement),
}

/// A schema change, as one DDL operation of the engine's Rust API takes it.
///
/// Names are as written: unqualified names are resolved through the
/// session's search path when the statement runs (ADR-0042).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DdlStatement {
    /// `(create-table NAME (column NAME TYPE)... [(primary-key NAME...)])`.
    CreateTable(TableSpec),
    /// `(drop-table NAME)`.
    DropTable(String),
    /// `(add-column TABLE NAME TYPE)`.
    AddColumn {
        /// The table.
        table: String,
        /// The new column.
        column: ColumnSpec,
    },
    /// `(create-index NAME :table TABLE KEY... [:unique B] [:nones-distinct B])`.
    CreateIndex(IndexSpec),
    /// `(drop-index NAME)`.
    DropIndex(String),
    /// `(declare-entity TYPE :table TABLE :key COLUMN (attr NAME [COLUMN] [:references TYPE])...)`.
    DeclareEntity(EntitySpec),
    /// `(drop-entity TYPE)`.
    DropEntity(String),
    /// `(declare-principal-type TYPE)`.
    DeclarePrincipalType(String),
    /// `(drop-principal-type TYPE)`.
    DropPrincipalType(String),
    /// `(declare-membership MEMBER GROUP :edges TABLE (M G) :closure TABLE (D A P))`.
    DeclareMembership(MembershipSpec),
    /// `(drop-membership MEMBER GROUP)`.
    DropMembership {
        /// The member entity type.
        member: String,
        /// The group entity type.
        group: String,
    },
    /// `(create-foreign-key NAME :table TABLE (columns C...) (references TABLE C...) [:on-delete A] [:match M])`.
    CreateForeignKey(ForeignKeySpec),
    /// `(drop-foreign-key NAME)`.
    DropForeignKey(String),
    /// `(set-leakproof FUNCTION (DATATYPE...) B)`.
    SetLeakproof {
        /// The function's name.
        function: String,
        /// The overload's argument types.
        args: Vec<DataType>,
        /// Whether it is leakproof.
        leakproof: bool,
    },
    /// `(create-namespace NAME :owner (entity TYPE KEY))`.
    CreateNamespace {
        /// The namespace.
        name: String,
        /// Its owner.
        owner: EntityUid,
    },
    /// `(drop-namespace NAME [:cascade B])`.
    DropNamespace {
        /// The namespace.
        name: String,
        /// Drop everything in it too.
        cascade: bool,
    },
    /// `(set-namespace-owner NAME (entity TYPE KEY))`.
    SetNamespaceOwner {
        /// The namespace.
        name: String,
        /// Its new owner.
        owner: EntityUid,
    },
}

impl DdlStatement {
    /// Return the statement's head word, such as `create-table`.
    #[must_use]
    pub fn form(&self) -> &'static str {
        match self {
            Self::CreateTable(_) => "create-table",
            Self::DropTable(_) => "drop-table",
            Self::AddColumn { .. } => "add-column",
            Self::CreateIndex(_) => "create-index",
            Self::DropIndex(_) => "drop-index",
            Self::DeclareEntity(_) => "declare-entity",
            Self::DropEntity(_) => "drop-entity",
            Self::DeclarePrincipalType(_) => "declare-principal-type",
            Self::DropPrincipalType(_) => "drop-principal-type",
            Self::DeclareMembership(_) => "declare-membership",
            Self::DropMembership { .. } => "drop-membership",
            Self::CreateForeignKey(_) => "create-foreign-key",
            Self::DropForeignKey(_) => "drop-foreign-key",
            Self::SetLeakproof { .. } => "set-leakproof",
            Self::CreateNamespace { .. } => "create-namespace",
            Self::DropNamespace { .. } => "drop-namespace",
            Self::SetNamespaceOwner { .. } => "set-namespace-owner",
        }
    }

    /// Return the name of the object the statement concerns, such as the
    /// table a `create-table` creates, for error messages.
    #[must_use]
    pub fn subject(&self) -> String {
        match self {
            Self::CreateTable(spec) => spec.name.clone(),
            Self::CreateIndex(spec) => spec.name.clone(),
            Self::DeclareEntity(spec) => spec.name.clone(),
            Self::DeclareMembership(spec) => format!("{} {}", spec.member, spec.group),
            Self::CreateForeignKey(spec) => spec.name.clone(),
            Self::DropTable(name)
            | Self::DropIndex(name)
            | Self::DropEntity(name)
            | Self::DeclarePrincipalType(name)
            | Self::DropPrincipalType(name)
            | Self::DropForeignKey(name)
            | Self::AddColumn { table: name, .. }
            | Self::SetLeakproof { function: name, .. }
            | Self::CreateNamespace { name, .. }
            | Self::DropNamespace { name, .. }
            | Self::SetNamespaceOwner { name, .. } => name.clone(),
            Self::DropMembership { member, group } => format!("{member} {group}"),
        }
    }
}
