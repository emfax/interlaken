//! Impact analysis and the forbid lint: the policy gates every node can run (ADR-0030).
//!
//! Impact analysis evaluates the current and proposed policies for every
//! principal, action and governed row in the data at hand, and every column
//! decision, and reports what would change. It covers today's data, not
//! every possible future row; proofs over all requests come from a prover
//! peer (ADR-0020).
//!
//! It scales with the change rather than the data where it can (ADR-0032):
//!
//! - **Pruning by resource type.** When both providers expose their
//!   policies, only the actions and resource types (and columns) of the
//!   policies that differ can change; every other decision is counted as
//!   pruned without being evaluated.
//! - **Incremental evaluation.** Under permit-unless-forbid, a decision
//!   depends only on which applicable policies hold. If none of the
//!   changed policies holds for a request, the same policies hold before
//!   and after, so the decision is unchanged; only requests where a
//!   changed policy holds are decided by both providers.
//! - **Sampling.** Above [`ImpactOptions::max_decisions`], principals and
//!   resources are each sampled (deterministically, by seed) so about that
//!   many decisions are compared, and the report says it was sampled.
//!
//! Opaque providers (Cedar's) are analysed in full, sampled above the
//! threshold.
//!
//! The forbid lint flags forbid policies whose condition can raise an
//! error (arithmetic, or attributes read through references, which fail
//! when the referenced entity is missing): a forbid that fails is ignored,
//! so it stops forbidding for that row.

use std::collections::BTreeSet;

use super::error::SecurityError;
use super::eval::{EntityStore, Env, ResourceData};
use super::expr::{Action, EntityUid, Policy, SecExpr, SecValue};
use super::provider::{Decision, PolicyProvider, Request, SecurityContext};
use super::store::DatabaseEntities;
use crate::catalog::Catalog;
use crate::interp::Database;
use crate::names::Ident;

/// One decision that a policy change would change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The principal.
    pub principal: EntityUid,
    /// The action.
    pub action: Action,
    /// The resource (for column changes, the resource type's first row is not meaningful; see `column`).
    pub resource: Option<EntityUid>,
    /// The resource type.
    pub resource_type: Ident,
    /// For a column-level change, the column.
    pub column: Option<Ident>,
}

/// What a policy change would change on the current data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImpactReport {
    /// Decisions that would change from deny to allow.
    pub newly_allowed: Vec<Change>,
    /// Decisions that would change from allow to deny.
    pub newly_denied: Vec<Change>,
    /// How many decisions the analysis established: compared, or proved
    /// unchanged because no changed policy holds for them.
    pub decisions: usize,
    /// How many of those both providers decided.
    pub evaluated: usize,
    /// How many decisions were known unchanged without evaluation: their
    /// action and resource type (or column) no changed policy covers.
    pub pruned: usize,
    /// How many decisions the changed policies cover, before sampling.
    pub population: usize,
    /// Whether principals and resources were sampled, so changes outside
    /// the sample are not reported.
    pub sampled: bool,
}

/// Settings for impact analysis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImpactOptions {
    /// Above this many decisions, principals and resources are sampled so
    /// about this many are compared.
    pub max_decisions: usize,
    /// The sampling seed: the same seed samples the same entities.
    pub seed: u64,
}

impl Default for ImpactOptions {
    /// Compare up to a million decisions, seed 0.
    fn default() -> Self {
        Self {
            max_decisions: 1_000_000,
            seed: 0,
        }
    }
}

impl ImpactReport {
    /// Return `true` if no decision changes.
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        self.newly_allowed.is_empty() && self.newly_denied.is_empty()
    }

    /// Return a short human-readable summary, listing up to `limit` changes of each kind.
    #[must_use]
    pub fn summary(&self, limit: usize) -> String {
        let show = |changes: &[Change]| -> Vec<String> {
            changes
                .iter()
                .take(limit)
                .map(|c| {
                    let target = match (&c.resource, &c.column) {
                        (_, Some(col)) => format!("column {}.{col}", c.resource_type),
                        (Some(r), None) => r.to_string(),
                        (None, None) => c.resource_type.to_string(),
                    };
                    format!("{} {} {target}", c.principal, c.action.name())
                })
                .collect()
        };
        let sampled = if self.sampled {
            format!(
                " (sampled from {} decisions; changes outside the sample are not shown)",
                self.population
            )
        } else {
            String::new()
        };
        format!(
            "{} decisions compared{sampled}; {} newly allowed [{}]; {} newly denied [{}]",
            self.decisions,
            self.newly_allowed.len(),
            show(&self.newly_allowed).join(", "),
            self.newly_denied.len(),
            show(&self.newly_denied).join(", ")
        )
    }
}

/// What a policy change can affect.
struct Scope {
    /// The (action, resource type) pairs whose row decisions can change.
    rows: BTreeSet<(Action, Ident)>,
    /// The (resource type, column) pairs whose column decisions can change.
    columns: BTreeSet<(Ident, Ident)>,
    /// The row-level policies in exactly one of the two sets, for
    /// incremental evaluation; `None` when a provider is opaque.
    changed: Option<Vec<Policy>>,
}

/// Return the policies in exactly one of `a` and `b`, counting duplicates.
fn difference(a: &[Policy], b: &[Policy]) -> Vec<Policy> {
    let mut rest: Vec<&Policy> = b.iter().collect();
    let mut out = Vec::new();
    for p in a {
        match rest.iter().position(|q| *q == p) {
            Some(i) => {
                rest.swap_remove(i);
            }
            None => out.push(p.clone()),
        }
    }
    out.extend(rest.into_iter().cloned());
    out
}

/// Return what changing `current` to `proposed` can affect.
fn scope(
    current: &dyn PolicyProvider,
    proposed: &dyn PolicyProvider,
    catalog: &dyn Catalog,
) -> Scope {
    let entities = catalog.entities();
    if let (Some(before), Some(after)) = (current.policies(), proposed.policies()) {
        let changed = difference(before, after);
        let mut scope = Scope {
            rows: BTreeSet::new(),
            columns: BTreeSet::new(),
            changed: None,
        };
        for p in &changed {
            match &p.column {
                None => scope
                    .rows
                    .extend(p.actions.iter().map(|a| (*a, p.resource_type.clone()))),
                Some(column) => {
                    scope
                        .columns
                        .insert((p.resource_type.clone(), column.clone()));
                }
            }
        }
        scope.changed = Some(changed.into_iter().filter(|p| p.column.is_none()).collect());
        return scope;
    }
    Scope {
        rows: entities
            .iter()
            .flat_map(|e| Action::ALL.map(|a| (a, e.name.clone())))
            .collect(),
        columns: entities
            .iter()
            .flat_map(|e| {
                e.attributes
                    .iter()
                    .map(|a| (e.name.clone(), a.name.clone()))
            })
            .collect(),
        changed: None,
    }
}

/// Return whether the sample at `rate` (of `u64::MAX`) for `seed` keeps `uid`.
fn kept(uid: &EntityUid, seed: u64, rate: u64) -> bool {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    uid.hash(&mut hasher);
    // SplitMix64's finalizer, so nearby seeds sample unrelated entities.
    let mut z = hasher.finish().wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z <= rate
}

/// Return whether a changed policy that applies to `action` on the
/// resource holds for `principal`: the only way the decision can change.
fn changed_policy_holds(
    changed: &[Policy],
    action: Action,
    resource: &ResourceData,
    context: &SecurityContext,
    store: &dyn EntityStore,
) -> bool {
    let record = context.record();
    let env = Env {
        principal: &context.principal,
        resource: Some(resource),
        context: &record,
        store,
    };
    changed
        .iter()
        .filter(|p| p.applies(action, &resource.ty))
        .any(|p| matches!(env.eval(&p.condition), Ok(SecValue::Bool(true))))
}

/// Compare `current` and `proposed` on every principal, action and governed
/// row of `db`, with the default [`ImpactOptions`].
///
/// # Errors
///
/// Returns a [`SecurityError`] if either provider fails to evaluate.
pub fn impact_analysis(
    current: &dyn PolicyProvider,
    proposed: &dyn PolicyProvider,
    catalog: &dyn Catalog,
    db: &Database,
    base: &SecurityContext,
) -> Result<ImpactReport, SecurityError> {
    impact_analysis_with(
        current,
        proposed,
        catalog,
        db,
        base,
        &ImpactOptions::default(),
    )
}

/// Compare `current` and `proposed` on every principal, action and governed
/// row of `db` that the change can affect, sampling above
/// `options.max_decisions`.
///
/// Every row of every entity type's table is a candidate principal; each
/// is evaluated with `base` (its principal replaced). Insert decisions are
/// evaluated on existing rows, which approximates rows yet to be written.
///
/// # Errors
///
/// Returns a [`SecurityError`] if either provider fails to evaluate.
#[allow(clippy::too_many_lines)]
pub fn impact_analysis_with(
    current: &dyn PolicyProvider,
    proposed: &dyn PolicyProvider,
    catalog: &dyn Catalog,
    db: &Database,
    base: &SecurityContext,
    options: &ImpactOptions,
) -> Result<ImpactReport, SecurityError> {
    let store = DatabaseEntities { catalog, db };
    let entities = catalog.entities();
    let mut resources = Vec::new();
    for entity in &entities {
        for row in db.rows(entity.table) {
            if let Some(resource) = store.resource(entity.table, row) {
                resources.push(resource);
            }
        }
    }
    let principals: Vec<EntityUid> = resources.iter().filter_map(|r| r.uid.clone()).collect();
    let scope = scope(current, proposed, catalog);
    let mut report = ImpactReport::default();
    // Decisions per principal, in and out of scope.
    let column_count: usize = entities.iter().map(|e| e.attributes.len()).sum();
    let in_scope = |r: &ResourceData, a: Action| scope.rows.contains(&(a, r.ty.clone()));
    let row_decisions: usize = Action::ALL
        .iter()
        .map(|&a| resources.iter().filter(|r| in_scope(r, a)).count())
        .sum();
    let per_principal = row_decisions + scope.columns.len();
    let all_per_principal = Action::ALL.len() * resources.len() + column_count;
    report.population = principals.len() * per_principal;
    report.pruned = principals.len() * (all_per_principal - per_principal);
    // Sample principals and resources each at the square root of the rate.
    let rate = if report.population > options.max_decisions {
        report.sampled = true;
        #[allow(clippy::cast_precision_loss)]
        let fraction = (options.max_decisions as f64 / report.population as f64).sqrt();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rate = (fraction * u64::MAX as f64) as u64;
        rate
    } else {
        u64::MAX
    };
    let sample =
        |uid: &EntityUid, salt: u64| rate == u64::MAX || kept(uid, options.seed ^ salt, rate);
    for principal in principals.iter().filter(|p| sample(p, 0)) {
        let mut context = base.clone();
        context.principal = principal.clone();
        for &action in &Action::ALL {
            for resource in &resources {
                if !in_scope(resource, action)
                    || !resource.uid.as_ref().is_none_or(|u| sample(u, 1))
                {
                    continue;
                }
                report.decisions += 1;
                if let Some(changed) = &scope.changed
                    && !changed_policy_holds(changed, action, resource, &context, &store)
                {
                    continue;
                }
                let req = Request {
                    context: &context,
                    action,
                    resource,
                };
                let (before, after) = (
                    current.authorize(&req, &store)?,
                    proposed.authorize(&req, &store)?,
                );
                report.evaluated += 1;
                let change = || Change {
                    principal: principal.clone(),
                    action,
                    resource: resource.uid.clone(),
                    resource_type: resource.ty.clone(),
                    column: None,
                };
                match (before, after) {
                    (Decision::Deny, Decision::Allow) => report.newly_allowed.push(change()),
                    (Decision::Allow, Decision::Deny) => report.newly_denied.push(change()),
                    _ => {}
                }
            }
        }
        for entity in &entities {
            for attribute in &entity.attributes {
                if !scope
                    .columns
                    .contains(&(entity.name.clone(), attribute.name.clone()))
                {
                    continue;
                }
                let before = current.column_access(
                    &context,
                    &entity.name,
                    &attribute.name,
                    &store as &dyn EntityStore,
                )?;
                let after =
                    proposed.column_access(&context, &entity.name, &attribute.name, &store)?;
                report.decisions += 1;
                report.evaluated += 1;
                let change = || Change {
                    principal: principal.clone(),
                    action: Action::Read,
                    resource: None,
                    resource_type: entity.name.clone(),
                    column: Some(attribute.name.clone()),
                };
                match (before, after) {
                    (Decision::Deny, Decision::Allow) => report.newly_allowed.push(change()),
                    (Decision::Allow, Decision::Deny) => report.newly_denied.push(change()),
                    _ => {}
                }
            }
        }
    }
    Ok(report)
}

/// Return why a forbid condition can raise an error, if it can.
///
/// Arithmetic can overflow, and an attribute read through a reference
/// (`resource.owner.dept`) fails when the referenced entity is missing.
#[must_use]
pub fn forbid_hazard(condition: &SecExpr) -> Option<&'static str> {
    match condition {
        SecExpr::Arith(..) => Some("arithmetic can overflow"),
        SecExpr::Attr(base, _) if matches!(**base, SecExpr::Attr(..)) => {
            Some("an attribute read through a reference fails if the entity is missing")
        }
        SecExpr::Try(..) => None,
        _ => children(condition).into_iter().find_map(forbid_hazard),
    }
}

/// Return the direct sub-expressions of `e`.
fn children(e: &SecExpr) -> Vec<&SecExpr> {
    match e {
        SecExpr::Attr(a, _)
        | SecExpr::Has(a, _)
        | SecExpr::Is(a, _)
        | SecExpr::Not(a)
        | SecExpr::Like(a, _) => vec![a],
        SecExpr::In(a, b)
        | SecExpr::Contains(a, b)
        | SecExpr::ContainsAll(a, b)
        | SecExpr::ContainsAny(a, b)
        | SecExpr::Cmp(_, a, b)
        | SecExpr::Arith(_, a, b)
        | SecExpr::And(a, b)
        | SecExpr::Or(a, b)
        | SecExpr::Try(a, b) => vec![a, b],
        SecExpr::If(c, a, b) => vec![c, a, b],
        _ => Vec::new(),
    }
}
