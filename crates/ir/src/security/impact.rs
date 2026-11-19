//! Impact analysis and the forbid lint: the policy gates every node can run (ADR-0030).
//!
//! Impact analysis evaluates the current and proposed policies for every
//! principal, action and governed row in the data at hand, and every column
//! decision, and reports what would change. It covers today's data, not
//! every possible future row; proofs over all requests come from a prover
//! peer (ADR-0020).
//!
//! The forbid lint flags forbid policies whose condition can raise an
//! error (arithmetic, or attributes read through references, which fail
//! when the referenced entity is missing): a forbid that fails is ignored,
//! so it stops forbidding for that row.

use super::error::SecurityError;
use super::eval::EntityStore;
use super::expr::{Action, EntityUid, SecExpr};
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
    /// How many decisions were compared.
    pub decisions: usize,
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
        format!(
            "{} decisions compared; {} newly allowed [{}]; {} newly denied [{}]",
            self.decisions,
            self.newly_allowed.len(),
            show(&self.newly_allowed).join(", "),
            self.newly_denied.len(),
            show(&self.newly_denied).join(", ")
        )
    }
}

/// Compare `current` and `proposed` on every principal, action and governed row of `db`.
///
/// Every row of every entity type's table is a candidate principal; each
/// is evaluated with `base` (its principal replaced). Insert decisions are
/// evaluated on existing rows, which approximates rows yet to be written.
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
    let mut report = ImpactReport::default();
    for principal in &principals {
        let mut context = base.clone();
        context.principal = principal.clone();
        for &action in &Action::ALL {
            for resource in &resources {
                let req = Request {
                    context: &context,
                    action,
                    resource,
                };
                let (before, after) = (
                    current.authorize(&req, &store)?,
                    proposed.authorize(&req, &store)?,
                );
                report.decisions += 1;
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
                let before = current.column_access(
                    &context,
                    &entity.name,
                    &attribute.name,
                    &store as &dyn EntityStore,
                )?;
                let after =
                    proposed.column_access(&context, &entity.name, &attribute.name, &store)?;
                report.decisions += 1;
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
