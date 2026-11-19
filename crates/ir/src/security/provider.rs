//! The security context and the policy provider contract (ADR-0015).

use std::collections::BTreeSet;

use super::error::SecurityError;
use super::eval::{EntityStore, Env, Partial, PartialEnv, ResourceData};
use super::expr::{Action, Effect, EntityUid, Policy, SecExpr, SecValue, bx};
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

    /// Return the policies covering `action` on `resource_type`, split into permits and forbids.
    fn applicable(&self, action: Action, resource_type: &Ident) -> (Vec<&Policy>, Vec<&Policy>) {
        self.policies
            .iter()
            .filter(|p| p.applies(action, resource_type))
            .partition(|p| p.effect == Effect::Permit)
    }
}

/// Combine partial results: `OR(permits) AND NOT OR(forbids)`.
fn combine(permits: Vec<Partial>, forbids: Vec<Partial>) -> Residual {
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

    /// A column no policy names is readable; one that some policy names is
    /// readable only if the column policies allow it for every row.
    /// A decision that depends on the row denies: per-row column masking is
    /// not supported, so this fails closed.
    fn column_access(
        &self,
        context: &SecurityContext,
        resource_type: &Ident,
        column: &Ident,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        let governing: Vec<&Policy> = self
            .policies
            .iter()
            .filter(|p| p.column.as_ref() == Some(column) && &p.resource_type == resource_type)
            .collect();
        if governing.is_empty() {
            return Ok(Decision::Allow);
        }
        let record = context.record();
        let penv = PartialEnv {
            env: Env { principal: &context.principal, resource: None, context: &record, store: entities },
            resource_type,
        };
        let (permits, forbids): (Vec<&Policy>, Vec<&Policy>) =
            governing.into_iter().filter(|p| p.actions.contains(&Action::Read)).partition(|p| p.effect == Effect::Permit);
        let eval = |ps: Vec<&Policy>| ps.into_iter().map(|p| penv.partial(&guarded(&p.condition))).collect();
        Ok(match combine(eval(permits), eval(forbids)) {
            Residual::Allow => Decision::Allow,
            Residual::Deny | Residual::Conditional(_) => Decision::Deny,
        })
    }
}
