//! The security context and the policy provider contract (ADR-0015).

use std::collections::BTreeSet;

use super::error::SecurityError;
use super::eval::{EntityStore, Env, Partial, PartialEnv, ResourceData};
use super::expr::{Action, Effect, EntityUid, Mask, Policy, SecExpr, SecValue, bx};
use super::paths::{EntityPath, collect_entity_paths};
use crate::names::Ident;

/// Facts about the request the engine fills in (ADR-0015).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StandardContext {
    /// Request time, microseconds since the Unix epoch.
    pub time: i64,
    /// Source IP address.
    pub ip: String,
    /// How the principal authenticated, for example `password` or `mfa`.
    pub auth_method: String,
    /// The session's ID.
    pub session_id: String,
}

/// Who is asking and in what circumstances; bound per transaction by the
/// engine, never supplied in client IR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecurityContext {
    /// The authenticated principal.
    pub principal: EntityUid,
    /// Engine-filled request facts.
    pub standard: StandardContext,
    /// Application-defined fields, under `context.app`.
    pub application: Vec<(Ident, SecValue)>,
}

impl SecurityContext {
    /// Construct a context for `principal` with empty request facts.
    #[must_use]
    pub fn new(principal: EntityUid) -> Self {
        Self {
            principal,
            standard: StandardContext::default(),
            application: Vec::new(),
        }
    }

    /// Return the context record policies see as `context`.
    #[must_use]
    pub fn record(&self) -> SecValue {
        SecValue::Record(vec![
            ("time".into(), SecValue::Long(self.standard.time)),
            ("ip".into(), SecValue::Str(self.standard.ip.clone())),
            (
                "auth_method".into(),
                SecValue::Str(self.standard.auth_method.clone()),
            ),
            (
                "session_id".into(),
                SecValue::Str(self.standard.session_id.clone()),
            ),
            ("app".into(), SecValue::Record(self.application.clone())),
        ])
    }
}

/// A request whose resource is unknown: what lowering asks before a query runs.
#[derive(Clone, Debug)]
pub struct PartialRequest<'a> {
    /// The security context.
    pub context: &'a SecurityContext,
    /// The action.
    pub action: Action,
    /// The resource's entity type.
    pub resource_type: &'a Ident,
}

/// A fully concrete request.
#[derive(Clone, Debug)]
pub struct Request<'a> {
    /// The security context.
    pub context: &'a SecurityContext,
    /// The action.
    pub action: Action,
    /// The resource.
    pub resource: &'a ResourceData,
}

/// The outcome of partial evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Residual {
    /// Every resource is allowed.
    Allow,
    /// No resource is allowed.
    Deny,
    /// A resource is allowed when the condition, over `resource`, is true.
    Conditional(SecExpr),
}

/// A concrete decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Allowed.
    Allow,
    /// Denied.
    Deny,
}

/// How individual policy results combine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CombiningRule {
    /// Allowed if some permit holds and no forbid holds; denied by default.
    /// Policies whose evaluation fails are ignored (Cedar's rule).
    PermitUnlessForbid,
}

/// How a principal may read one column of a resource type (ADR-0018).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnAccess {
    /// Every row's value is readable.
    Allow,
    /// Reading the column fails the query.
    Deny,
    /// Rows where `visible` (a condition over `resource`) holds show the
    /// value; the others show `mask` applied to it (ADR-0032).
    Masked {
        /// When the value is shown; `false` to mask every row.
        visible: SecExpr,
        /// What the other rows show.
        mask: Mask,
    },
}

/// What a residual depends on, for caching it (ADR-0019).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dependencies {
    /// The context fields read (`time`, `ip`, `app.tenant`, ...).
    pub context_fields: BTreeSet<String>,
    /// Whether the principal's attributes or memberships were read.
    pub principal_data: bool,
}

/// A source of policies the engine enforces (ADR-0015).
pub trait PolicyProvider: Send + Sync {
    /// Return the condition under which `req`'s principal may perform its
    /// action on a resource of its type.
    ///
    /// # Errors
    ///
    /// Returns a [`SecurityError`] if the policies cannot be evaluated.
    fn partial_eval(
        &self,
        req: &PartialRequest<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Residual, SecurityError>;

    /// Decide a fully concrete request, for writes.
    ///
    /// # Errors
    ///
    /// Returns a [`SecurityError`] if the policies cannot be evaluated.
    fn authorize(
        &self,
        req: &Request<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError>;

    /// Return what the residual for `req` depends on.
    fn dependencies(&self, req: &PartialRequest<'_>) -> Dependencies;

    /// Return how policy results combine.
    fn combining_rule(&self) -> CombiningRule;

    /// Return warnings about policies whose forbid conditions can raise
    /// errors and so stop forbidding (ADR-0030). None by default.
    fn lint(&self) -> Vec<String> {
        Vec::new()
    }

    /// Decide whether the principal may read attribute `column` of
    /// resources of type `resource_type` (ADR-0018). Must be decidable from
    /// the principal and context alone.
    ///
    /// # Errors
    ///
    /// Returns a [`SecurityError`] if the policies cannot be evaluated.
    fn column_access(
        &self,
        context: &SecurityContext,
        resource_type: &Ident,
        column: &Ident,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError>;

    /// Decide how the principal may read attribute `column` of resources of
    /// type `resource_type`, row by row: unlike [`column_access`], a
    /// decision that depends on the row masks the column if its policies
    /// declare a mask (ADR-0032). By default, `column_access`'s decision.
    ///
    /// # Errors
    ///
    /// Returns a [`SecurityError`] if the policies cannot be evaluated or
    /// declare conflicting masks.
    ///
    /// [`column_access`]: PolicyProvider::column_access
    fn column_mask(
        &self,
        context: &SecurityContext,
        resource_type: &Ident,
        column: &Ident,
        entities: &dyn EntityStore,
    ) -> Result<ColumnAccess, SecurityError> {
        Ok(
            match self.column_access(context, resource_type, column, entities)? {
                Decision::Allow => ColumnAccess::Allow,
                Decision::Deny => ColumnAccess::Deny,
            },
        )
    }

    /// Return the entities other than the principal that the policies read
    /// (`principal.manager.dept`), which must be loaded before partial
    /// evaluation (ADR-0032). None by default.
    fn entity_paths(&self) -> Vec<EntityPath> {
        Vec::new()
    }

    /// Return the policies, if the provider holds them as sublanguage
    /// policies, so impact analysis can compare policy sets policy by
    /// policy (ADR-0030). `None` by default: the provider is opaque.
    fn policies(&self) -> Option<&[Policy]> {
        None
    }
}

/// Return how `governing` (the column policies of one column) let the
/// principal read the column row by row: their combined residual and the
/// mask they declare.
fn column_policies(
    governing: &[&Policy],
    context: &SecurityContext,
    resource_type: &Ident,
    entities: &dyn EntityStore,
) -> (Residual, Vec<Mask>) {
    let record = context.record();
    let penv = PartialEnv {
        env: Env {
            principal: &context.principal,
            resource: None,
            context: &record,
            store: entities,
        },
        resource_type,
    };
    let readers: Vec<&Policy> = governing
        .iter()
        .copied()
        .filter(|p| p.actions.contains(&Action::Read))
        .collect();
    let mut masks: Vec<Mask> = Vec::new();
    for mask in readers.iter().filter_map(|p| p.mask.clone()) {
        if !masks.contains(&mask) {
            masks.push(mask);
        }
    }
    let (permits, forbids): (Vec<&Policy>, Vec<&Policy>) = readers
        .into_iter()
        .partition(|p| p.effect == Effect::Permit);
    let eval = |ps: Vec<&Policy>| {
        ps.into_iter()
            .map(|p| penv.partial(&guarded(&p.condition)))
            .collect()
    };
    (combine(eval(permits), eval(forbids)), masks)
}

/// Return the column access a residual and the declared masks give.
///
/// # Errors
///
/// Returns [`SecurityError::Invalid`] if the masks conflict.
pub(crate) fn mask_access(
    residual: Residual,
    masks: Vec<Mask>,
    resource_type: &Ident,
    column: &Ident,
) -> Result<ColumnAccess, SecurityError> {
    let mut masks = masks.into_iter();
    let (mask, other) = (masks.next(), masks.next());
    if other.is_some() {
        return Err(SecurityError::Invalid(format!(
            "column {column} of {resource_type} has conflicting masks"
        )));
    }
    Ok(match (residual, mask) {
        (Residual::Allow, _) => ColumnAccess::Allow,
        (Residual::Deny | Residual::Conditional(_), None) => ColumnAccess::Deny,
        (Residual::Deny, Some(mask)) => ColumnAccess::Masked {
            visible: SecExpr::Lit(SecValue::Bool(false)),
            mask,
        },
        (Residual::Conditional(visible), Some(mask)) => ColumnAccess::Masked { visible, mask },
    })
}

/// A Postgres row-level-security-style provider: policies are predicates in the sublanguage.
#[derive(Clone, Debug, Default)]
pub struct RlsProvider {
    /// The policies.
    policies: Vec<Policy>,
}

/// Collect the context fields and principal reads in `e`.
fn collect_dependencies(e: &SecExpr, out: &mut Dependencies) {
    /// Return the dotted context path of an attribute chain on `context`.
    fn context_path(e: &SecExpr) -> Option<String> {
        match e {
            SecExpr::Context => Some(String::new()),
            SecExpr::Attr(base, attr) | SecExpr::Has(base, attr) => context_path(base).map(|p| {
                if p.is_empty() {
                    attr.to_string()
                } else {
                    format!("{p}.{attr}")
                }
            }),
            _ => None,
        }
    }
    match e {
        SecExpr::Attr(base, _) | SecExpr::Has(base, _) if context_path(base).is_some() => {
            out.context_fields
                .insert(context_path(e).expect("context path"));
            return;
        }
        SecExpr::Attr(base, _) | SecExpr::Has(base, _) if matches!(**base, SecExpr::Principal) => {
            out.principal_data = true;
        }
        SecExpr::In(a, _) if matches!(**a, SecExpr::Principal) => out.principal_data = true,
        _ => {}
    }
    let children: Vec<&SecExpr> = match e {
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
    };
    children
        .into_iter()
        .for_each(|c| collect_dependencies(c, out));
}

impl RlsProvider {
    /// Construct a provider enforcing `policies`.
    #[must_use]
    pub fn new(policies: Vec<Policy>) -> Self {
        Self { policies }
    }

    /// Return the column-level policies governing `column` of `resource_type`.
    fn governing(&self, resource_type: &Ident, column: &Ident) -> Vec<&Policy> {
        self.policies
            .iter()
            .filter(|p| p.column.as_ref() == Some(column) && &p.resource_type == resource_type)
            .collect()
    }

    /// Return the policies covering `action` on `resource_type`, split into permits and forbids.
    fn applicable(&self, action: Action, resource_type: &Ident) -> (Vec<&Policy>, Vec<&Policy>) {
        self.policies
            .iter()
            .filter(|p| p.applies(action, resource_type))
            .partition(|p| p.effect == Effect::Permit)
    }
}

/// Combine partial results: `OR(permits) AND NOT OR(forbids)`.
pub(crate) fn combine(permits: Vec<Partial>, forbids: Vec<Partial>) -> Residual {
    /// Fold partial results with `||`, dropping known-false ones.
    fn any(parts: Vec<Partial>) -> Partial {
        let mut residuals = Vec::new();
        for p in parts {
            match p {
                Partial::Known(SecValue::Bool(true)) => {
                    return Partial::Known(SecValue::Bool(true));
                }
                Partial::Residual(r) => residuals.push(r),
                // Each part is a Try(..., false), so it cannot fail or be non-boolean.
                _ => {}
            }
        }
        let mut iter = residuals.into_iter();
        match iter.next() {
            None => Partial::Known(SecValue::Bool(false)),
            Some(first) => {
                Partial::Residual(iter.fold(first, |acc, r| SecExpr::Or(bx(acc), bx(r))))
            }
        }
    }
    let (permitted, forbidden) = (any(permits), any(forbids));
    match (permitted, forbidden) {
        (Partial::Known(SecValue::Bool(false)), _) | (_, Partial::Known(SecValue::Bool(true))) => {
            Residual::Deny
        }
        (Partial::Known(SecValue::Bool(true)), Partial::Known(SecValue::Bool(false))) => {
            Residual::Allow
        }
        (p, Partial::Known(SecValue::Bool(false))) => Residual::Conditional(p.residualize()),
        (Partial::Known(SecValue::Bool(true)), f) => {
            Residual::Conditional(SecExpr::Not(bx(f.residualize())))
        }
        (p, f) => Residual::Conditional(SecExpr::And(
            bx(p.residualize()),
            bx(SecExpr::Not(bx(f.residualize()))),
        )),
    }
}

/// Wrap a policy condition so a failure counts as false (ADR-0016).
fn guarded(condition: &SecExpr) -> SecExpr {
    SecExpr::Try(
        bx(condition.clone()),
        bx(SecExpr::Lit(SecValue::Bool(false))),
    )
}

impl PolicyProvider for RlsProvider {
    /// Partially evaluate the applicable policies and combine them.
    fn partial_eval(
        &self,
        req: &PartialRequest<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Residual, SecurityError> {
        let context = req.context.record();
        let penv = PartialEnv {
            env: Env {
                principal: &req.context.principal,
                resource: None,
                context: &context,
                store: entities,
            },
            resource_type: req.resource_type,
        };
        let (permits, forbids) = self.applicable(req.action, req.resource_type);
        let eval = |ps: Vec<&Policy>| {
            ps.into_iter()
                .map(|p| penv.partial(&guarded(&p.condition)))
                .collect()
        };
        Ok(combine(eval(permits), eval(forbids)))
    }

    /// Evaluate the applicable policies concretely: allowed if a permit holds and no forbid does.
    fn authorize(
        &self,
        req: &Request<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        let context = req.context.record();
        let env = Env {
            principal: &req.context.principal,
            resource: Some(req.resource),
            context: &context,
            store: entities,
        };
        let (permits, forbids) = self.applicable(req.action, &req.resource.ty);
        let holds = |p: &&Policy| matches!(env.eval(&p.condition), Ok(SecValue::Bool(true)));
        let allowed = permits.iter().any(holds) && !forbids.iter().any(holds);
        Ok(if allowed {
            Decision::Allow
        } else {
            Decision::Deny
        })
    }

    /// Return the context fields and principal data the applicable policies read.
    fn dependencies(&self, req: &PartialRequest<'_>) -> Dependencies {
        let mut out = Dependencies::default();
        for policy in self
            .policies
            .iter()
            .filter(|p| p.applies(req.action, req.resource_type))
        {
            collect_dependencies(&policy.condition, &mut out);
        }
        out
    }

    /// Permit unless forbidden, default deny.
    fn combining_rule(&self) -> CombiningRule {
        CombiningRule::PermitUnlessForbid
    }

    /// Flag forbid policies whose condition can raise an error.
    fn lint(&self) -> Vec<String> {
        self.policies
            .iter()
            .enumerate()
            .filter(|(_, p)| p.effect == Effect::Forbid)
            .filter_map(|(i, p)| {
                super::impact::forbid_hazard(&p.condition)
                    .map(|why| format!("forbid policy {i} on {}: {why}", p.resource_type))
            })
            .collect()
    }

    /// A column no policy names is readable; one that some policy names is
    /// readable only if the column policies allow it for every row.
    /// A decision that depends on the row denies (fail closed); see
    /// [`PolicyProvider::column_mask`] for masking such columns instead.
    fn column_access(
        &self,
        context: &SecurityContext,
        resource_type: &Ident,
        column: &Ident,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        let governing = self.governing(resource_type, column);
        if governing.is_empty() {
            return Ok(Decision::Allow);
        }
        Ok(
            match column_policies(&governing, context, resource_type, entities).0 {
                Residual::Allow => Decision::Allow,
                Residual::Deny | Residual::Conditional(_) => Decision::Deny,
            },
        )
    }

    /// As [`RlsProvider::column_access`], but a row-dependent decision
    /// masks the column when its policies declare a mask.
    fn column_mask(
        &self,
        context: &SecurityContext,
        resource_type: &Ident,
        column: &Ident,
        entities: &dyn EntityStore,
    ) -> Result<ColumnAccess, SecurityError> {
        let governing = self.governing(resource_type, column);
        if governing.is_empty() {
            return Ok(ColumnAccess::Allow);
        }
        let (residual, masks) = column_policies(&governing, context, resource_type, entities);
        mask_access(residual, masks, resource_type, column)
    }

    /// Return the entities every policy, row-level or column-level, reads.
    fn entity_paths(&self) -> Vec<EntityPath> {
        let mut out = BTreeSet::new();
        for p in &self.policies {
            collect_entity_paths(&p.condition, &mut out);
        }
        out.into_iter().collect()
    }

    /// Return the policies.
    fn policies(&self) -> Option<&[Policy]> {
        Some(&self.policies)
    }
}
