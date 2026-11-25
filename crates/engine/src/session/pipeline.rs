//! The statement pipeline: IR text to a physical plan (ADR-0031).
//!
//! Every query goes through the same stages, in one transaction:
//!
//! 1. parse the text (or take a built plan);
//! 2. resolve it against the transaction's catalog, unless it is already
//!    resolved (pinned to a schema version);
//! 3. verify it as untrusted client IR, which rejects a plan pinned to an
//!    older schema version and any privileged operator;
//! 4. load the active policies, then the principal's entity data and that
//!    of every other entity the policies read (ADR-0017, ADR-0032), through
//!    the transaction;
//! 5. secure it with the policies' provider, its residuals cached
//!    (ADR-0019);
//! 6. optimize it;
//! 7. plan it physically.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use interlaken_exec::{CascadePlans, ExecContext, Physical, PlanOptions, QueryMemory, plan_with};
use interlaken_ir::Plan;
use interlaken_ir::catalog::{Catalog, InMemoryCatalog, cascade_plan, reachable_cascades};
use interlaken_ir::ddl::{DdlStatement, Statement};
use interlaken_ir::interp::{Datum, ExecOptions, declared_params};
use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::security::{SecurityContext, secure};
use interlaken_ir::text::parse_statement;
use interlaken_ir::verify::{VerifyOptions, verify};
use interlaken_ir::{RelSchema, Type};
use interlaken_storage::Txn;

use super::engine::Inner;
use crate::authz::load_entities;
use crate::error::EngineError;
use crate::policy::{load_policies, policy_settings};
use crate::privileges::Privileges;

/// Everything a statement runs with.
pub(crate) struct Step<'a> {
    /// The engine's shared state.
    pub(crate) engine: &'a Inner,
    /// The transaction.
    pub(crate) txn: &'a Txn,
    /// The catalog as the transaction sees it.
    pub(crate) catalog: Arc<InMemoryCatalog>,
    /// The session's principal and request facts.
    pub(crate) context: &'a SecurityContext,
    /// The principal's privileges, read through the transaction.
    pub(crate) privileges: Privileges,
    /// Whether the shared caches apply: not after DDL or a policy change
    /// in this transaction, whose versions aren't committed.
    pub(crate) use_caches: bool,
    /// Evaluation settings (`now()`, the random seed) for the transaction.
    pub(crate) options: ExecOptions,
    /// When the statement times out, if it has a timeout.
    pub(crate) deadline: Option<Instant>,
    /// The namespaces unqualified names are looked up in, in order (ADR-0042).
    pub(crate) search_path: &'a [String],
}

/// A statement ready to run.
pub(crate) struct Prepared {
    /// The physical plan.
    pub(crate) physical: Physical,
    /// The output column names.
    pub(crate) columns: Vec<String>,
    /// The output column types.
    pub(crate) types: Vec<Type>,
    /// The type each parameter is declared with, by number.
    pub(crate) params: BTreeMap<u32, Type>,
    /// The secured cascade of each foreign key the statement's deletes can
    /// set off, by name (ADR-0035).
    pub(crate) cascades: Arc<CascadePlans>,
}

/// Parse a plan's text, refusing DDL, which has no plan (ADR-0043).
pub(crate) fn parse(text: &str) -> Result<Plan, EngineError> {
    match parse_statement(text).map_err(EngineError::Syntax)? {
        Statement::Plan(plan) => Ok(plan),
        Statement::Ddl(ddl) => Err(not_a_plan(&ddl)),
    }
}

/// Return the error for DDL where only a plan can go.
pub(crate) fn not_a_plan(ddl: &DdlStatement) -> EngineError {
    EngineError::NotSupported(format!(
        "({} ...) is DDL, which has no plan; run it with Session::query or Session::execute_script",
        ddl.form()
    ))
}

/// Resolve `plan` (unless already resolved), looking unqualified names up
/// through `search_path`, and verify it as client IR.
pub(crate) fn pin(
    plan: &Plan,
    catalog: &dyn Catalog,
    search_path: &[String],
) -> Result<(Plan, RelSchema), EngineError> {
    let resolved = if plan.schema_version.is_some() {
        plan.clone()
    } else {
        let scoped = crate::catalog::Scoped {
            inner: catalog,
            path: search_path,
        };
        resolve(plan, &scoped)?.plan
    };
    let schema = verify(&resolved, catalog, &VerifyOptions { trusted: false })?;
    Ok((resolved, schema))
}

/// Take `plan` through the pipeline to a physical plan.
pub(crate) async fn prepare(step: &Step<'_>, plan: &Plan) -> Result<Prepared, EngineError> {
    let catalog = step.catalog.as_ref();
    let (resolved, schema) = pin(plan, catalog, step.search_path)?;
    let params = declared_params(&resolved.root);
    let policies = load_policies(step.txn).await?;
    let scopes = crate::policy::load_scopes(step.txn).await?;
    let settings = policy_settings(step.txn).await?;
    let provider = step.engine.provider(
        &step.catalog,
        &policies,
        &scopes,
        &settings.app_context,
        step.use_caches,
    )?;
    let entities = load_entities(
        step.txn,
        catalog,
        &step.context.principal,
        &provider.entity_paths(),
    )
    .await?;
    let cached = step.engine.residuals.provider(
        provider.as_ref(),
        step.engine.policy_key(&policies, &scopes),
    );
    let provider: &dyn interlaken_ir::security::PolicyProvider = if step.use_caches {
        &cached
    } else {
        provider.as_ref()
    };
    let physical = |resolved: &Plan| -> Result<Physical, EngineError> {
        let secured = secure(resolved, catalog, provider, step.context, &entities)?;
        let optimized = optimize(&secured, catalog);
        Ok(plan_with(&optimized, catalog, PlanOptions::default())?)
    };
    // Each cascade is an ordinary statement, secured for the same principal,
    // so policies decide which rows it may change.
    let mut cascades = CascadePlans::new();
    for fk in reachable_cascades(catalog, &resolved.root) {
        if let Some(cascade) = cascade_plan(catalog, &fk) {
            let (cascade, _) = pin(&cascade, catalog, &[])?;
            cascades.insert(fk.name.to_string(), Arc::new(physical(&cascade)?));
        }
    }
    let physical = physical(&resolved)?;
    let columns = schema
        .columns
        .iter()
        .map(|c| {
            c.name
                .as_ref()
                .map_or_else(|| "?column?".to_owned(), ToString::to_string)
        })
        .collect();
    let types = schema.columns.iter().map(|c| c.ty.clone()).collect();
    Ok(Prepared {
        physical,
        columns,
        types,
        params,
        cascades: Arc::new(cascades),
    })
}

/// Return the context `prepared` runs in, with `params`.
pub(crate) fn exec_context(
    step: &Step<'_>,
    prepared: &Prepared,
    params: Vec<Datum>,
) -> ExecContext {
    let catalog: Arc<dyn Catalog> = step.catalog.clone();
    let ctx = ExecContext::new(step.txn.clone(), catalog, params, step.options)
        .with_memory(QueryMemory::new(
            step.engine.query_budget,
            step.engine.pool.clone(),
        ))
        .with_cascades(prepared.cascades.clone());
    match step.deadline {
        Some(deadline) => ctx.with_deadline(deadline),
        None => ctx,
    }
}
