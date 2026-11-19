//! The SMT prover: proof gates with Cedar SymCC and cvc5 (ADR-0020, ADR-0030).
//!
//! Only prover nodes build this (feature `smt-prover`) and configure the
//! path of a cvc5 1.3.1 executable. For every request environment in the
//! generated schema (principal type, action, resource type), it asks the
//! solver for a request and entities on which:
//!
//! - a proposed policy raises an error (never-errors);
//! - the proposed set allows what the current set denies (no broadening);
//! - the proposed set allows what a declared invariant does not (invariants).
//!
//! An answer is a counterexample, recorded with the failed gate; no answer
//! is a proof over every well-formed request. Column-level policies
//! (`@column`) are checked for errors but take no part in row-level
//! broadening and invariants.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use cedar_policy::{PolicySet, Schema};
use cedar_policy_symcc::solver::LocalSolver;
use cedar_policy_symcc::{CedarSymCompiler, CompiledPolicy, CompiledPolicySet, CompiledSchema};
use interlaken_ir::catalog::Catalog;
use interlaken_ir::security::cedar_schema;

use super::document::{PolicyDocument, PolicyLanguage};
use super::proposals::Proposal;
use super::prover::Prover;
use super::verify::{Gate, GateResult};
use crate::error::EngineError;

/// A prover running cvc5 through Cedar SymCC.
#[derive(Clone, Debug)]
pub struct SmtProver {
    /// The name recorded with results.
    name: String,
    /// The cvc5 executable.
    cvc5: PathBuf,
    /// The solver's time limit per query, in milliseconds.
    time_limit_ms: u64,
}

/// Construct a prover error.
fn prover_error(what: &str, e: impl std::fmt::Display) -> EngineError {
    EngineError::Invalid(format!("prover {what}: {e}"))
}

/// Return the row-level policies of `set` (those without a `@column` annotation).
fn row_policies(set: &PolicySet) -> Result<PolicySet, EngineError> {
    let mut out = PolicySet::new();
    for policy in set.policies().filter(|p| p.annotation("column").is_none()) {
        out.add(policy.clone())
            .map_err(|e| prover_error("policy set", e))?;
    }
    Ok(out)
}

impl SmtProver {
    /// Construct a prover named `name` using the cvc5 executable at `cvc5`.
    #[must_use]
    pub fn new(name: impl Into<String>, cvc5: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            cvc5: cvc5.into(),
            time_limit_ms: 60_000,
        }
    }

    /// Run the proofs.
    async fn run(
        &self,
        current: &PolicyDocument,
        proposal: &Proposal,
        catalog: &Arc<dyn Catalog>,
        app_context: &str,
    ) -> Result<Vec<GateResult>, EngineError> {
        let names: Vec<String> = catalog
            .entities()
            .iter()
            .map(|e| e.name.to_string())
            .collect();
        let types: Vec<&str> = names.iter().map(String::as_str).collect();
        let schema_text = cedar_schema(catalog.as_ref(), &types, app_context)
            .map_err(|e| prover_error("schema", e))?;
        let (schema, _) =
            Schema::from_cedarschema_str(&schema_text).map_err(|e| prover_error("schema", e))?;
        let parse = |text: &str| PolicySet::from_str(text).map_err(|e| prover_error("policies", e));
        let proposed = parse(&proposal.document.body)?;
        let (new_rows, old_rows) = (
            row_policies(&proposed)?,
            row_policies(&parse(&current.body)?)?,
        );
        let invariants = if proposal.invariants.trim().is_empty() {
            None
        } else {
            Some(parse(&proposal.invariants)?)
        };

        let mut command = tokio::process::Command::new(&self.cvc5);
        command.args(["--lang", "smt", &format!("--tlimit={}", self.time_limit_ms)]);
        let solver =
            LocalSolver::from_command(&mut command).map_err(|e| prover_error("solver", e))?;
        let mut compiler = CedarSymCompiler::new(solver).map_err(|e| prover_error("solver", e))?;
        let compiled_schema =
            CompiledSchema::new(&schema).map_err(|e| prover_error("schema", e))?;

        let (mut errors, mut broadening, mut violations) = (None, None, None);
        for env in schema.request_envs() {
            let compile_set = |set: &PolicySet| {
                CompiledPolicySet::compile(set, &env, &compiled_schema)
                    .map_err(|e| prover_error("compile", e))
            };
            if errors.is_none() {
                for policy in proposed.policies() {
                    let compiled = CompiledPolicy::compile(policy, &env, &compiled_schema)
                        .map_err(|e| prover_error("compile", e))?;
                    if let Some(cex) = compiler
                        .check_never_errors_with_counterexample_opt(&compiled)
                        .await
                        .map_err(|e| prover_error("solve", e))?
                    {
                        errors = Some(format!(
                            "policy {} raises an error for {}: {cex}",
                            policy.id(),
                            env.action()
                        ));
                        break;
                    }
                }
            }
            let new_compiled = compile_set(&new_rows)?;
            if broadening.is_none() {
                let old_compiled = compile_set(&old_rows)?;
                if let Some(cex) = compiler
                    .check_implies_with_counterexample_opt(&new_compiled, &old_compiled)
                    .await
                    .map_err(|e| prover_error("solve", e))?
                {
                    broadening = Some(format!("newly allowed {}: {cex}", env.action()));
                }
            }
            if let (Some(invariants), None) = (&invariants, &violations) {
                let invariant_compiled = compile_set(invariants)?;
                if let Some(cex) = compiler
                    .check_implies_with_counterexample_opt(&new_compiled, &invariant_compiled)
                    .await
                    .map_err(|e| prover_error("solve", e))?
                {
                    violations = Some(format!(
                        "allowed beyond the invariants for {}: {cex}",
                        env.action()
                    ));
                }
            }
        }
        let result = |gate, found: Option<String>, proved: &str| match found {
            Some(cex) => GateResult::new(gate, false, cex),
            None => GateResult::new(gate, true, proved),
        };
        Ok(vec![
            result(
                Gate::NeverErrors,
                errors,
                "proved: no policy raises an error",
            ),
            result(
                Gate::NoBroadening,
                broadening,
                "proved: nothing newly allowed",
            ),
            result(
                Gate::Invariants,
                violations,
                if invariants.is_some() {
                    "proved: within the invariants"
                } else {
                    "no invariants declared"
                },
            ),
        ])
    }
}

impl Prover for SmtProver {
    /// Return the prover's name.
    fn name(&self) -> &str {
        &self.name
    }

    /// Prove the change, or record why it cannot be proved (non-Cedar policies).
    async fn prove(
        &self,
        current: &PolicyDocument,
        proposal: &Proposal,
        catalog: &Arc<dyn Catalog>,
        app_context: &str,
    ) -> Result<Vec<GateResult>, EngineError> {
        if current.language != PolicyLanguage::Cedar
            || proposal.document.language != PolicyLanguage::Cedar
        {
            let why = "proofs need Cedar policies on both sides";
            return Ok([Gate::NeverErrors, Gate::NoBroadening, Gate::Invariants]
                .map(|g| GateResult::new(g, false, why))
                .to_vec());
        }
        self.run(current, proposal, catalog, app_context).await
    }
}
