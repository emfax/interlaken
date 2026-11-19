//! The Cedar provider: TPE for partial evaluation, Cedar's authorizer for writes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use cedar_policy::{
    Context, Decision as CedarDecision, Entity, EntityId, EntityLoader, EntityTypeName,
    EntityUid as CedarUid, PartialEntities, PartialEntity, PartialEntityUid,
    PartialRequest as CedarPartialRequest, PolicySet, Request as CedarRequest,
    RestrictedExpression, Schema, ValidationMode, Validator, pst,
};
use smol_str::SmolStr;

use super::schema::cedar_schema;
use super::translate::{Translator, eid};
use crate::catalog::Catalog;
use crate::names::Ident;
use crate::security::error::SecurityError;
use crate::security::eval::{EntityStore, Env, Partial, PartialEnv, ResourceData};
use crate::security::expr::{Action, EntityUid, SecExpr, SecValue, bx};
use crate::security::provider::{
    CombiningRule, Decision, Dependencies, PartialRequest, PolicyProvider, Request, Residual,
    SecurityContext, combine,
};

/// The annotation marking a column-level policy: `@column("secret")`.
const COLUMN_ANNOTATION: &str = "column";

/// Construct a Cedar error.
fn cedar_error(what: &str, e: impl std::fmt::Display) -> SecurityError {
    SecurityError::Invalid(format!("Cedar {what}: {e}"))
}

/// A [`PolicyProvider`] backed by Cedar policies (ADR-0020).
///
/// Policies are Cedar text over the schema generated from the catalog,
/// with actions `Action::"read"`, `"insert"`, `"update"` and `"delete"`.
/// Policies annotated `@column("name")` govern reading that attribute
/// (ADR-0018) and take no part in row-level decisions.
pub struct CedarProvider {
    /// The catalog, for entity key types in residuals.
    catalog: std::sync::Arc<dyn Catalog>,
    /// The generated schema.
    schema: Schema,
    /// Row-level policies.
    rows: PolicySet,
    /// Column-level policies, by column name.
    columns: HashMap<String, PolicySet>,
    /// Every policy, for dependency analysis.
    all: PolicySet,
}

impl std::fmt::Debug for CedarProvider {
    /// Show the policy count.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CedarProvider")
            .field("policies", &self.all.policies().count())
            .finish_non_exhaustive()
    }
}

/// Return the Cedar UID of an entity.
fn cedar_uid(uid: &EntityUid) -> Result<CedarUid, SecurityError> {
    let ty =
        EntityTypeName::from_str(uid.ty.as_str()).map_err(|e| cedar_error("entity type", e))?;
    Ok(CedarUid::from_type_name_and_id(
        ty,
        EntityId::new(eid(&uid.key)),
    ))
}

/// Return the Cedar UID of an action.
fn action_uid(action: Action) -> Result<CedarUid, SecurityError> {
    CedarUid::from_str(&format!("Action::{:?}", action.name()))
        .map_err(|e| cedar_error("action", e))
}

/// Convert a sublanguage value to a Cedar expression.
fn restricted(v: &SecValue) -> Result<RestrictedExpression, SecurityError> {
    Ok(match v {
        SecValue::Bool(b) => RestrictedExpression::new_bool(*b),
        SecValue::Long(n) => RestrictedExpression::new_long(*n),
        SecValue::Str(s) => RestrictedExpression::new_string(s.clone()),
        SecValue::Entity(uid) => RestrictedExpression::new_entity_uid(cedar_uid(uid)?),
        SecValue::Set(items) => RestrictedExpression::new_set(
            items
                .iter()
                .map(restricted)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        SecValue::Record(fields) => RestrictedExpression::new_record(
            fields
                .iter()
                .map(|(k, v)| Ok((k.to_string(), restricted(v)?)))
                .collect::<Result<Vec<_>, SecurityError>>()?,
        )
        .map_err(|e| cedar_error("record", e))?,
    })
}

/// Return the Cedar context for a security context.
fn cedar_context(context: &SecurityContext) -> Result<Context, SecurityError> {
    let SecValue::Record(fields) = context.record() else {
        unreachable!("the context is a record")
    };
    Context::from_pairs(
        fields
            .iter()
            .map(|(k, v)| Ok((k.to_string(), restricted(v)?)))
            .collect::<Result<Vec<_>, SecurityError>>()?,
    )
    .map_err(|e| cedar_error("context", e))
}

/// Return the principal's entity for partial evaluation: attributes and ancestors if it exists.
fn principal_entity(
    principal: &EntityUid,
    store: &dyn EntityStore,
    schema: &Schema,
) -> Result<Option<PartialEntity>, SecurityError> {
    let Some(attributes) = store.attributes(principal) else {
        return Ok(None);
    };
    let attrs: BTreeMap<SmolStr, RestrictedExpression> = attributes
        .iter()
        .map(|(k, v)| Ok((SmolStr::from(k.as_str()), restricted(v)?)))
        .collect::<Result<_, SecurityError>>()?;
    let ancestors: HashSet<CedarUid> = store
        .ancestors(principal)
        .iter()
        .map(cedar_uid)
        .collect::<Result<_, _>>()?;
    PartialEntity::new(
        cedar_uid(principal)?,
        Some(attrs),
        Some(ancestors),
        Some(BTreeMap::new()),
        schema,
    )
    .map(Some)
    .map_err(|e| cedar_error("principal entity", e))
}

/// Loads entities on demand for Cedar's authorizer, from an entity store and the request's resource.
struct Loader<'a> {
    /// The entity data.
    store: &'a dyn EntityStore,
    /// The resource, which may not be stored yet.
    resource: &'a ResourceData,
    /// The resource's Cedar UID.
    resource_uid: CedarUid,
    /// Our UIDs by Cedar UID, for entities seen so far.
    known: HashMap<CedarUid, EntityUid>,
}

impl Loader<'_> {
    /// Build a Cedar entity from attributes and ancestors.
    fn entity(
        uid: &CedarUid,
        attributes: &[(Ident, SecValue)],
        ancestors: &[EntityUid],
    ) -> Option<Entity> {
        let attrs = attributes
            .iter()
            .map(|(k, v)| Some((k.to_string(), restricted(v).ok()?)))
            .collect::<Option<HashMap<_, _>>>()?;
        let parents = ancestors
            .iter()
            .map(|a| cedar_uid(a).ok())
            .collect::<Option<HashSet<_>>>()?;
        Entity::new(uid.clone(), attrs, parents).ok()
    }

    /// Return our UID for a Cedar UID, by parsing its type and ID.
    fn ours(&self, uid: &CedarUid) -> Option<EntityUid> {
        self.known.get(uid).cloned()
    }
}

impl EntityLoader for Loader<'_> {
    /// Load each requested entity with its attributes and every ancestor.
    fn load_entities(&mut self, uids: &HashSet<CedarUid>) -> HashMap<CedarUid, Option<Entity>> {
        uids.iter()
            .map(|uid| {
                if *uid == self.resource_uid {
                    let ancestors = self
                        .resource
                        .uid
                        .as_ref()
                        .map(|u| self.store.ancestors(u))
                        .unwrap_or_default();
                    return (
                        uid.clone(),
                        Self::entity(uid, &self.resource.attributes, &ancestors),
                    );
                }
                let entity = self.ours(uid).and_then(|ours| {
                    let attributes = self.store.attributes(&ours)?;
                    Self::entity(uid, &attributes, &self.store.ancestors(&ours))
                });
                (uid.clone(), entity)
            })
            .collect()
    }
}

/// Collect the entity UIDs mentioned anywhere in `value`.
fn entities_in(value: &SecValue, out: &mut Vec<EntityUid>) {
    match value {
        SecValue::Entity(uid) => out.push(uid.clone()),
        SecValue::Set(items) => items.iter().for_each(|v| entities_in(v, out)),
        SecValue::Record(fields) => fields.iter().for_each(|(_, v)| entities_in(v, out)),
        _ => {}
    }
}

impl CedarProvider {
    /// Construct a provider for Cedar `policies`, validating them against the
    /// schema generated from `catalog` for `entity_types`, with `app_context`
    /// as the Cedar record type of `context.app`.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::Invalid`] if the schema or policies do not
    /// parse, or the policies do not validate (Cedar's strict mode).
    pub fn new(
        policies: &str,
        catalog: std::sync::Arc<dyn Catalog>,
        entity_types: &[&str],
        app_context: &str,
    ) -> Result<Self, SecurityError> {
        let schema_text = cedar_schema(catalog.as_ref(), entity_types, app_context)?;
        let (schema, _) =
            Schema::from_cedarschema_str(&schema_text).map_err(|e| cedar_error("schema", e))?;
        let all = PolicySet::from_str(policies).map_err(|e| cedar_error("policies", e))?;
        let validation = Validator::new(schema.clone()).validate(&all, ValidationMode::Strict);
        if !validation.validation_passed() {
            let errors: Vec<String> = validation
                .validation_errors()
                .map(ToString::to_string)
                .collect();
            return Err(SecurityError::Invalid(format!(
                "Cedar policies do not validate: {}",
                errors.join("; ")
            )));
        }
        let mut rows = PolicySet::new();
        let mut columns: HashMap<String, PolicySet> = HashMap::new();
        for policy in all.policies() {
            let set = match policy.annotation(COLUMN_ANNOTATION) {
                Some(column) => columns.entry(column.to_owned()).or_default(),
                None => &mut rows,
            };
            set.add(policy.clone())
                .map_err(|e| cedar_error("policy set", e))?;
        }
        Ok(Self {
            catalog,
            schema,
            rows,
            columns,
            all,
        })
    }

    /// Return the Cedar schema generated from the catalog, for diagnostics and analysis.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Run TPE on `set` for a request whose resource is unknown.
    fn partial(
        &self,
        set: &PolicySet,
        context: &SecurityContext,
        action: Action,
        resource_type: &Ident,
        store: &dyn EntityStore,
    ) -> Result<Residual, SecurityError> {
        let resource_type_name = EntityTypeName::from_str(resource_type.as_str())
            .map_err(|e| cedar_error("resource type", e))?;
        let request = CedarPartialRequest::new(
            PartialEntityUid::from_concrete(cedar_uid(&context.principal)?),
            action_uid(action)?,
            PartialEntityUid::new(resource_type_name, None),
            Some(cedar_context(context)?),
            &self.schema,
        )
        .map_err(|e| cedar_error("partial request", e))?;
        let entities = PartialEntities::from_partial_entities(
            principal_entity(&context.principal, store, &self.schema)?,
            &self.schema,
        )
        .map_err(|e| cedar_error("entities", e))?;
        let response = set
            .tpe(&request, &entities, &self.schema)
            .map_err(|e| cedar_error("partial evaluation", e))?;
        match response.decision() {
            Some(CedarDecision::Allow) => return Ok(Residual::Allow),
            Some(CedarDecision::Deny) => return Ok(Residual::Deny),
            None => {}
        }
        // Translate the residuals, then let our own partial evaluator substitute
        // the principal and context and simplify, exactly as for other providers.
        let record = context.record();
        let translator = Translator {
            catalog: self.catalog.as_ref(),
            principal: &context.principal,
            context: &record,
        };
        let penv = PartialEnv {
            env: Env {
                principal: &context.principal,
                resource: None,
                context: &record,
                store,
            },
            resource_type,
        };
        let condition = |id: &cedar_policy::PolicyId| -> Result<Partial, SecurityError> {
            let policy = response
                .get_policy(id)
                .ok_or_else(|| cedar_error("residual", "missing policy"))?;
            let pst::Policy::Static(body) =
                policy.to_pst().map_err(|e| cedar_error("residual", e))?
            else {
                return Err(cedar_error("residual", "a linked template"));
            };
            let mut parts = Vec::new();
            for clause in body.body().clauses() {
                parts.push(match clause {
                    pst::Clause::When(e) => translator.expr(e)?,
                    pst::Clause::Unless(e) => SecExpr::Not(bx(translator.expr(e)?)),
                });
            }
            let condition = parts
                .into_iter()
                .reduce(|a, b| SecExpr::And(bx(a), bx(b)))
                .unwrap_or(SecExpr::Lit(SecValue::Bool(true)));
            Ok(penv.partial(&SecExpr::Try(
                bx(condition),
                bx(SecExpr::Lit(SecValue::Bool(false))),
            )))
        };
        let mut permits: Vec<Partial> = response
            .true_permits()
            .map(|_| Partial::Known(SecValue::Bool(true)))
            .collect();
        for id in response.residual_permits() {
            permits.push(condition(id)?);
        }
        let mut forbids: Vec<Partial> = response
            .true_forbids()
            .map(|_| Partial::Known(SecValue::Bool(true)))
            .collect();
        for id in response.residual_forbids() {
            forbids.push(condition(id)?);
        }
        Ok(combine(permits, forbids))
    }
}

impl PolicyProvider for CedarProvider {
    /// Partially evaluate the row-level policies with Cedar's TPE and translate the residual.
    fn partial_eval(
        &self,
        req: &PartialRequest<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Residual, SecurityError> {
        self.partial(
            &self.rows,
            req.context,
            req.action,
            req.resource_type,
            entities,
        )
    }

    /// Decide with Cedar's own authorizer, loading entities on demand.
    fn authorize(
        &self,
        req: &Request<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        let resource_uid = match &req.resource.uid {
            Some(uid) => cedar_uid(uid)?,
            None => cedar_uid(&EntityUid::str(req.resource.ty.as_str(), ""))?,
        };
        let request = CedarRequest::new(
            cedar_uid(&req.context.principal)?,
            action_uid(req.action)?,
            resource_uid.clone(),
            cedar_context(req.context)?,
            Some(&self.schema),
        )
        .map_err(|e| cedar_error("request", e))?;
        // Every entity reachable from the request, keyed by Cedar UID, so the loader can map back.
        let mut reachable = vec![req.context.principal.clone()];
        reachable.extend(entities.ancestors(&req.context.principal));
        for (_, v) in &req.resource.attributes {
            entities_in(v, &mut reachable);
        }
        let mut known = HashMap::new();
        let mut frontier = reachable;
        while let Some(uid) = frontier.pop() {
            let cedar = cedar_uid(&uid)?;
            if known.insert(cedar, uid.clone()).is_some() {
                continue;
            }
            for (_, v) in entities.attributes(&uid).unwrap_or_default() {
                entities_in(&v, &mut frontier);
            }
            frontier.extend(entities.ancestors(&uid));
        }
        if let Some(uid) = &req.resource.uid {
            for a in entities.ancestors(uid) {
                known.insert(cedar_uid(&a)?, a);
            }
        }
        let mut loader = Loader {
            store: entities,
            resource: req.resource,
            resource_uid,
            known,
        };
        let decision = self
            .rows
            .is_authorized_batched(&request, &self.schema, &mut loader, 16)
            .map_err(|e| cedar_error("authorization", e))?;
        Ok(match decision {
            CedarDecision::Allow => Decision::Allow,
            CedarDecision::Deny => Decision::Deny,
        })
    }

    /// Return the context fields and principal data any policy reads.
    fn dependencies(&self, _req: &PartialRequest<'_>) -> Dependencies {
        let mut out = Dependencies::default();
        /// Collect dependencies from one expression.
        fn walk(e: &pst::Expr, out: &mut Dependencies) {
            /// Return the dotted context path of an attribute chain on `context`.
            fn path(e: &pst::Expr) -> Option<String> {
                match e {
                    pst::Expr::Var(pst::Var::Context) => Some(String::new()),
                    pst::Expr::GetAttr { expr, attr } => path(expr).map(|p| {
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
                pst::Expr::GetAttr { .. } if path(e).is_some_and(|p| !p.is_empty()) => {
                    out.context_fields.insert(path(e).expect("context path"));
                    return;
                }
                pst::Expr::GetAttr { expr, .. } | pst::Expr::HasAttr { expr, .. }
                    if matches!(**expr, pst::Expr::Var(pst::Var::Principal)) =>
                {
                    out.principal_data = true;
                }
                pst::Expr::BinaryOp {
                    op: pst::BinaryOp::In,
                    left,
                    ..
                } if matches!(**left, pst::Expr::Var(pst::Var::Principal)) => {
                    out.principal_data = true;
                }
                _ => {}
            }
            match e {
                pst::Expr::UnaryOp { expr, .. }
                | pst::Expr::GetAttr { expr, .. }
                | pst::Expr::HasAttr { expr, .. }
                | pst::Expr::Like { expr, .. } => walk(expr, out),
                pst::Expr::BinaryOp { left, right, .. } => {
                    walk(left, out);
                    walk(right, out);
                }
                pst::Expr::Is { expr, in_expr, .. } => {
                    walk(expr, out);
                    if let Some(i) = in_expr {
                        walk(i, out);
                    }
                }
                pst::Expr::IfThenElse {
                    cond,
                    then_expr,
                    else_expr,
                } => {
                    walk(cond, out);
                    walk(then_expr, out);
                    walk(else_expr, out);
                }
                pst::Expr::Set(items) => items.iter().for_each(|i| walk(i, out)),
                pst::Expr::Record(fields) => fields.values().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        if let Ok(set) = self.all.to_pst() {
            for policy in set.policies.values() {
                if matches!(
                    policy.body().principal,
                    pst::PrincipalConstraint::In(_) | pst::PrincipalConstraint::IsIn(..)
                ) {
                    out.principal_data = true;
                }
                for clause in policy.body().clauses() {
                    let (pst::Clause::When(e) | pst::Clause::Unless(e)) = clause;
                    walk(e.as_ref(), &mut out);
                }
            }
        }
        out
    }

    /// Flag forbid policies whose condition can raise an error, so they would stop forbidding.
    fn lint(&self) -> Vec<String> {
        /// Return why `e` can raise an error, if it can.
        fn hazard(e: &pst::Expr) -> Option<&'static str> {
            match e {
                pst::Expr::BinaryOp {
                    op: pst::BinaryOp::Add | pst::BinaryOp::Sub | pst::BinaryOp::Mul,
                    ..
                } => Some("arithmetic can overflow"),
                pst::Expr::GetAttr { expr, .. } if matches!(**expr, pst::Expr::GetAttr { .. }) => {
                    Some("an attribute read through a reference fails if the entity is missing")
                }
                pst::Expr::UnaryOp { expr, .. }
                | pst::Expr::GetAttr { expr, .. }
                | pst::Expr::HasAttr { expr, .. }
                | pst::Expr::Like { expr, .. } => hazard(expr),
                pst::Expr::BinaryOp { left, right, .. } => hazard(left).or_else(|| hazard(right)),
                pst::Expr::Is { expr, in_expr, .. } => {
                    hazard(expr).or_else(|| in_expr.as_deref().and_then(hazard))
                }
                pst::Expr::IfThenElse {
                    cond,
                    then_expr,
                    else_expr,
                } => hazard(cond)
                    .or_else(|| hazard(then_expr))
                    .or_else(|| hazard(else_expr)),
                pst::Expr::Set(items) => items.iter().find_map(|i| hazard(i)),
                pst::Expr::Record(fields) => fields.values().find_map(|v| hazard(v)),
                _ => None,
            }
        }
        let Ok(set) = self.all.to_pst() else {
            return vec!["policies could not be inspected".into()];
        };
        set.policies
            .iter()
            .filter(|(_, p)| matches!(p.body().effect, pst::Effect::Forbid))
            .filter_map(|(id, p)| {
                p.body()
                    .clauses()
                    .iter()
                    .find_map(|c| {
                        let (pst::Clause::When(e) | pst::Clause::Unless(e)) = c;
                        hazard(e)
                    })
                    .map(|why| format!("forbid policy {}: {why}", id.0))
            })
            .collect()
    }

    /// Permit unless forbidden, default deny: Cedar's rule.
    fn combining_rule(&self) -> CombiningRule {
        CombiningRule::PermitUnlessForbid
    }

    /// Allow a column no `@column` policy names; otherwise only if those policies allow every row.
    fn column_access(
        &self,
        context: &SecurityContext,
        resource_type: &Ident,
        column: &Ident,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        let Some(set) = self.columns.get(column.as_str()) else {
            return Ok(Decision::Allow);
        };
        Ok(
            match self.partial(set, context, Action::Read, resource_type, entities)? {
                Residual::Allow => Decision::Allow,
                Residual::Deny | Residual::Conditional(_) => Decision::Deny,
            },
        )
    }
}
