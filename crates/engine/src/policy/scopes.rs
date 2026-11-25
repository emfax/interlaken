//! Policy sets per namespace, beside the database-wide baseline (ADR-0042).

use std::collections::BTreeMap;
use std::sync::Arc;

use interlaken_ir::catalog::Catalog;
use interlaken_ir::security::{PolicyProvider, RlsProvider, ScopedProvider, parse_policies};
use interlaken_storage::Txn;
use interlaken_storage::catalog::{self, ColumnDef, TableDef};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::rows::{self, RowScan};

use super::document::{ActivePolicies, PolicyDocument, PolicyLanguage, build_provider};
use super::tables::load_policies;
use crate::EngineError;

/// Namespaces' policy sets: scope, version and text.
pub(crate) const SCOPES_TABLE: &str = "engine_policy_scopes";
/// The scope of each proposal for a namespace's set; others are for the baseline.
pub(crate) const PROPOSAL_SCOPES_TABLE: &str = "engine_policy_proposal_scopes";

/// One namespace's active policy set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedPolicies {
    /// The namespace.
    pub scope: String,
    /// Its version: 0 before any activation.
    pub version: i64,
    /// Its policies, in the baseline's language.
    pub body: String,
}

/// Return `name`'s table, creating it as `columns` keyed by `key` if a
/// database made before ADR-0042 lacks it.
async fn table(
    txn: &Txn,
    name: &str,
    columns: Vec<ColumnDef>,
    key: &str,
) -> Result<TableDef, EngineError> {
    if let Some(table) = catalog::table(txn, name).await? {
        return Ok(table);
    }
    Ok(catalog::create_table(
        txn,
        catalog::TableSpec {
            name: name.into(),
            columns,
            primary_key: vec![key.into()],
        },
    )
    .await?)
}

/// Return the scopes table.
async fn scopes_table(txn: &Txn) -> Result<TableDef, EngineError> {
    let text = |n: &str| ColumnDef::new(n, ColumnType::Text).not_null();
    table(
        txn,
        SCOPES_TABLE,
        vec![
            text("scope"),
            ColumnDef::new("version", ColumnType::Int8).not_null(),
            text("body"),
        ],
        "scope",
    )
    .await
}

/// Return the proposal scopes table.
async fn proposal_scopes_table(txn: &Txn) -> Result<TableDef, EngineError> {
    table(
        txn,
        PROPOSAL_SCOPES_TABLE,
        vec![
            ColumnDef::new("proposal", ColumnType::Int8).not_null(),
            ColumnDef::new("scope", ColumnType::Text).not_null(),
        ],
        "proposal",
    )
    .await
}

/// Return every row of `table`.
async fn all_rows(txn: &Txn, table: &TableDef) -> Result<Vec<Vec<Datum>>, EngineError> {
    let mut out = Vec::new();
    let mut scan = RowScan::all();
    loop {
        let batch = rows::scan(txn, table, &scan).await?;
        out.extend(batch.rows);
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => return Ok(out),
        }
    }
}

/// Return the text at `i`.
fn text(row: &[Datum], i: usize) -> Result<String, EngineError> {
    match row.get(i) {
        Some(Datum::Text(t)) => Ok(t.clone()),
        _ => Err(EngineError::Catalog("malformed policy scope row".into())),
    }
}

/// Return every namespace's active policy set, ordered by namespace. Reading
/// them registers each as a read, so a concurrent activation conflicts.
///
/// # Errors
///
/// Returns a storage error.
pub async fn load_scopes(txn: &Txn) -> Result<Vec<ScopedPolicies>, EngineError> {
    let Some(table) = catalog::table(txn, SCOPES_TABLE).await? else {
        return Ok(Vec::new());
    };
    all_rows(txn, &table)
        .await?
        .iter()
        .map(|row| {
            Ok(ScopedPolicies {
                scope: text(row, 0)?,
                version: match row.get(1) {
                    Some(Datum::Int8(v)) => *v,
                    _ => return Err(EngineError::Catalog("malformed policy scope row".into())),
                },
                body: text(row, 2)?,
            })
        })
        .collect()
}

/// Return namespace `scope`'s active set, or version 0 and no policies.
///
/// # Errors
///
/// Returns a storage error.
pub async fn load_scope(txn: &Txn, scope: &str) -> Result<ScopedPolicies, EngineError> {
    Ok(load_scopes(txn)
        .await?
        .into_iter()
        .find(|s| s.scope == scope)
        .unwrap_or(ScopedPolicies {
            scope: scope.to_owned(),
            version: 0,
            body: String::new(),
        }))
}

/// Make `body` namespace `scope`'s active set at `version`.
pub(crate) async fn write_scope(
    txn: &Txn,
    scope: &str,
    version: i64,
    body: &str,
) -> Result<(), EngineError> {
    let table = scopes_table(txn).await?;
    let row = vec![
        Datum::Text(scope.to_owned()),
        Datum::Int8(version),
        Datum::Text(body.to_owned()),
    ];
    if !rows::update(txn, &table, &[Datum::Text(scope.to_owned())], row.clone()).await? {
        rows::insert(txn, &table, row).await?;
    }
    Ok(())
}

/// Remove namespace `scope`'s policy set, when the namespace is dropped.
///
/// # Errors
///
/// Returns a storage error.
pub(crate) async fn drop_policy_scope(txn: &Txn, scope: &str) -> Result<(), EngineError> {
    if let Some(table) = catalog::table(txn, SCOPES_TABLE).await? {
        rows::delete(txn, &table, &[Datum::Text(scope.to_owned())]).await?;
    }
    Ok(())
}

/// Record that proposal `id` is for namespace `scope`'s set.
pub(crate) async fn set_proposal_scope(txn: &Txn, id: i64, scope: &str) -> Result<(), EngineError> {
    let table = proposal_scopes_table(txn).await?;
    rows::insert(
        txn,
        &table,
        vec![Datum::Int8(id), Datum::Text(scope.to_owned())],
    )
    .await?;
    Ok(())
}

/// Return the namespace proposal `id` is for, or `None` for the baseline.
///
/// # Errors
///
/// Returns a storage error.
pub async fn proposal_scope(txn: &Txn, id: i64) -> Result<Option<String>, EngineError> {
    let Some(table) = catalog::table(txn, PROPOSAL_SCOPES_TABLE).await? else {
        return Ok(None);
    };
    rows::get(txn, &table, &[Datum::Int8(id)])
        .await?
        .map(|row| text(&row, 1))
        .transpose()
}

/// Build the provider for one namespace: the baseline's policies and the
/// namespace's `body` together, in the baseline's language.
fn scope_provider(
    baseline: &PolicyDocument,
    body: &str,
    catalog: &Arc<dyn Catalog>,
    app_context: &str,
) -> Result<Box<dyn PolicyProvider>, EngineError> {
    match baseline.language {
        PolicyLanguage::Cedar => build_provider(
            &PolicyDocument {
                language: PolicyLanguage::Cedar,
                body: format!("{}\n{body}", baseline.body),
            },
            Arc::clone(catalog),
            app_context,
        ),
        PolicyLanguage::Rls => {
            let mut policies = parse_policies(&baseline.body)?;
            if !body.trim().is_empty() {
                policies.extend(parse_policies(body)?);
            }
            Ok(Box::new(RlsProvider::new(policies)))
        }
    }
}

/// Which policy set a proposal would replace, to build the provider it
/// would give.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Replacing<'a> {
    /// Nothing: the active policies.
    Nothing,
    /// The baseline, with this document.
    Baseline(&'a PolicyDocument),
    /// A namespace's set, with this text.
    Scope(&'a str, &'a str),
}

/// Build the provider for `baseline` and `scopes`, with one set replaced
/// as `replacing` says: a provider routing each namespace with a set to
/// the baseline and that set together, and everything else to the baseline.
///
/// # Errors
///
/// Returns the error of a set that doesn't parse or validate.
pub(crate) fn effective(
    baseline: &ActivePolicies,
    scopes: &[ScopedPolicies],
    replacing: Replacing<'_>,
    catalog: Arc<dyn Catalog>,
    app_context: &str,
) -> Result<Box<dyn PolicyProvider>, EngineError> {
    let baseline_document = match replacing {
        Replacing::Baseline(document) => document,
        _ => &baseline.document,
    };
    let mut bodies: BTreeMap<&str, &str> = scopes
        .iter()
        .map(|s| (s.scope.as_str(), s.body.as_str()))
        .collect();
    if let Replacing::Scope(scope, body) = replacing {
        bodies.insert(scope, body);
    }
    let base = build_provider(baseline_document, Arc::clone(&catalog), app_context)?;
    if bodies.values().all(|body| body.trim().is_empty()) {
        return Ok(base);
    }
    let mut routed = BTreeMap::new();
    for (scope, body) in bodies {
        if body.trim().is_empty() {
            continue;
        }
        let provider = scope_provider(baseline_document, body, &catalog, app_context)?;
        routed.insert(scope.to_owned(), Arc::from(provider));
    }
    Ok(Box::new(ScopedProvider::new(Arc::from(base), routed)))
}

/// Check that every policy in `body`, a set for namespace `scope` in
/// `language`, can only concern resources of `scope`'s types (ADR-0042).
///
/// # Errors
///
/// Returns [`EngineError::Invalid`] naming the first policy that could
/// concern another namespace's resources, or the parse error.
pub(crate) fn check_confined(
    language: PolicyLanguage,
    scope: &str,
    body: &str,
) -> Result<(), EngineError> {
    let own = |ty: &str| ty.split_once('.').is_some_and(|(n, _)| n == scope);
    let outside = |what: String| {
        EngineError::Invalid(format!(
            "a policy for namespace {scope} must concern only its own types: {what}"
        ))
    };
    match language {
        PolicyLanguage::Rls => {
            for policy in parse_policies(body)? {
                if !own(policy.resource_type.as_str()) {
                    return Err(outside(format!("resource type {}", policy.resource_type)));
                }
            }
        }
        PolicyLanguage::Cedar => {
            let types = interlaken_ir::security::policy_resource_types(body)
                .map_err(|e| EngineError::Invalid(e.to_string()))?;
            for ty in types {
                match ty {
                    None => return Err(outside("a policy names no resource type".into())),
                    Some(ty) if !own(&ty) => return Err(outside(format!("resource type {ty}"))),
                    Some(_) => {}
                }
            }
        }
    }
    Ok(())
}

/// Return the baseline and every namespace's set.
///
/// # Errors
///
/// Returns a storage error.
pub(crate) async fn load_all(
    txn: &Txn,
) -> Result<(ActivePolicies, Vec<ScopedPolicies>), EngineError> {
    Ok((load_policies(txn).await?, load_scopes(txn).await?))
}
