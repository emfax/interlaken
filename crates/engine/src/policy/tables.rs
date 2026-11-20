//! The policy tables and the active policy set.

use interlaken_storage::Txn;
use interlaken_storage::catalog::{self, ColumnDef, TableDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::rows;

use super::document::{ActivePolicies, PolicyDocument, PolicyLanguage};
use crate::error::EngineError;

/// The table holding the active policy set: one row.
pub const POLICY_TABLE: &str = "engine_policy_set";
/// The table holding policy settings: one row.
pub(crate) const SETTINGS_TABLE: &str = "engine_policy_settings";
/// The table of proposed policy changes.
pub(crate) const PROPOSALS_TABLE: &str = "engine_policy_proposals";
/// The table of gate results recorded by verifiers.
pub(crate) const VERIFICATIONS_TABLE: &str = "engine_policy_verifications";

/// What a proposal needs before it can be activated (ADR-0030).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerificationMode {
    /// A prover node must prove the change; proposals wait until one does.
    /// Requires Cedar policies.
    ProofRequired,
    /// Validation, the forbid lint and impact analysis on local data
    /// suffice, with a confirmation for anything they flag. The only mode
    /// for an embedded database.
    AnalysisSufficient,
}

impl VerificationMode {
    /// Return the stored name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ProofRequired => "proof-required",
            Self::AnalysisSufficient => "analysis-sufficient",
        }
    }

    /// Parse a stored name.
    fn from_name(name: &str) -> Result<Self, EngineError> {
        match name {
            "proof-required" => Ok(Self::ProofRequired),
            "analysis-sufficient" => Ok(Self::AnalysisSufficient),
            other => Err(EngineError::Catalog(format!(
                "unknown verification mode {other}"
            ))),
        }
    }
}

/// Cluster-wide policy settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicySettings {
    /// What activation requires.
    pub mode: VerificationMode,
    /// The Cedar record type of `context.app`, for example `{ "tenant": String }`.
    pub app_context: String,
}

/// Return a text datum.
fn text(s: &str) -> Datum {
    Datum::Text(s.into())
}

/// Read a text column.
pub(crate) fn text_of(d: &Datum) -> Result<&str, EngineError> {
    match d {
        Datum::Text(s) => Ok(s),
        other => Err(EngineError::Catalog(format!(
            "expected text, found {other:?}"
        ))),
    }
}

/// Read an integer column.
pub(crate) fn int_of(d: &Datum) -> Result<i64, EngineError> {
    match d {
        Datum::Int8(v) => Ok(*v),
        other => Err(EngineError::Catalog(format!(
            "expected an integer, found {other:?}"
        ))),
    }
}

/// Return a policy table's definition.
pub(crate) async fn table(txn: &Txn, name: &str) -> Result<TableDef, EngineError> {
    catalog::table(txn, name)
        .await?
        .ok_or_else(|| EngineError::Catalog(format!("table {name} does not exist")))
}

/// Create the policy tables, with an empty Cedar policy set (deny everything) at version 0.
///
/// # Errors
///
/// Returns an [`EngineError`] if the tables exist or storage fails.
pub async fn create_policy_tables(txn: &Txn, settings: &PolicySettings) -> Result<(), EngineError> {
    let int = |n: &str| ColumnDef::new(n, ColumnType::Int8).not_null();
    let txt = |n: &str| ColumnDef::new(n, ColumnType::Text).not_null();
    let specs = [
        (
            POLICY_TABLE,
            vec![int("id"), int("version"), txt("language"), txt("body")],
            vec!["id"],
        ),
        (
            SETTINGS_TABLE,
            vec![int("id"), txt("mode"), txt("app_context")],
            vec!["id"],
        ),
        (
            PROPOSALS_TABLE,
            vec![
                int("id"),
                int("base_version"),
                txt("language"),
                txt("body"),
                txt("invariants"),
                txt("proposer"),
                txt("state"),
                ColumnDef::new("activated_version", ColumnType::Int8),
                ColumnDef::new("confirmed_by", ColumnType::Text),
            ],
            vec!["id"],
        ),
        (
            VERIFICATIONS_TABLE,
            vec![
                int("proposal"),
                txt("verifier"),
                txt("gate"),
                ColumnDef::new("passed", ColumnType::Bool).not_null(),
                txt("detail"),
            ],
            vec!["proposal", "verifier", "gate"],
        ),
    ];
    for (name, columns, pk) in specs {
        let spec = TableSpec {
            name: name.into(),
            columns,
            primary_key: pk.into_iter().map(Into::into).collect(),
        };
        catalog::create_table(txn, spec).await?;
    }
    let policies = table(txn, POLICY_TABLE).await?;
    rows::insert(
        txn,
        &policies,
        vec![
            Datum::Int8(1),
            Datum::Int8(0),
            text(PolicyLanguage::Cedar.name()),
            text(""),
        ],
    )
    .await?;
    let settings_table = table(txn, SETTINGS_TABLE).await?;
    rows::insert(
        txn,
        &settings_table,
        vec![
            Datum::Int8(1),
            text(settings.mode.name()),
            text(&settings.app_context),
        ],
    )
    .await?;
    txn.next_statement();
    Ok(())
}

/// Read the policy settings.
///
/// # Errors
///
/// Returns an [`EngineError`] if the table is missing or malformed.
pub async fn policy_settings(txn: &Txn) -> Result<PolicySettings, EngineError> {
    let table = table(txn, SETTINGS_TABLE).await?;
    let row = rows::get(txn, &table, &[Datum::Int8(1)])
        .await?
        .ok_or_else(|| EngineError::Catalog("the policy settings row is missing".into()))?;
    Ok(PolicySettings {
        mode: VerificationMode::from_name(text_of(&row[1])?)?,
        app_context: text_of(&row[2])?.to_owned(),
    })
}

/// Replace the policy settings.
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails.
pub async fn set_policy_settings(txn: &Txn, settings: &PolicySettings) -> Result<(), EngineError> {
    let table = table(txn, SETTINGS_TABLE).await?;
    rows::update(
        txn,
        &table,
        &[Datum::Int8(1)],
        vec![
            Datum::Int8(1),
            text(settings.mode.name()),
            text(&settings.app_context),
        ],
    )
    .await?;
    txn.next_statement();
    Ok(())
}

/// Read the active policy set, registering a dependency on it so a writing
/// transaction conflicts with a concurrent activation at every isolation level.
///
/// # Errors
///
/// Returns an [`EngineError`] if the table is missing or malformed.
pub async fn load_policies(txn: &Txn) -> Result<ActivePolicies, EngineError> {
    let table = table(txn, POLICY_TABLE).await?;
    let pk = [Datum::Int8(1)];
    let row = rows::get(txn, &table, &pk)
        .await?
        .ok_or_else(|| EngineError::Catalog("the policy set row is missing".into()))?;
    rows::register_row_dependency(txn, &table, &pk)?;
    Ok(ActivePolicies {
        version: int_of(&row[1])?,
        document: PolicyDocument {
            language: PolicyLanguage::from_name(text_of(&row[2])?)?,
            body: text_of(&row[3])?.to_owned(),
        },
    })
}

/// Replace the active policy set with `document` at `version`. Only activation calls this.
pub(crate) async fn write_policies(
    txn: &Txn,
    version: i64,
    document: &PolicyDocument,
) -> Result<(), EngineError> {
    let table = table(txn, POLICY_TABLE).await?;
    rows::update(
        txn,
        &table,
        &[Datum::Int8(1)],
        vec![
            Datum::Int8(1),
            Datum::Int8(version),
            text(document.language.name()),
            text(&document.body),
        ],
    )
    .await?;
    Ok(())
}
