//! Policy sets per namespace (ADR-0042).

use std::collections::BTreeMap;
use std::sync::Arc;

use super::error::SecurityError;
use super::eval::EntityStore;
use super::paths::EntityPath;
use super::provider::{
    ColumnAccess, CombiningRule, Decision, Dependencies, PartialRequest, PolicyProvider, Request,
    Residual, SecurityContext,
};
use crate::Ident;

/// Return the namespace of entity type `ty`: `app` for `app.Todo`, or
/// `None` for a type in `public`.
#[must_use]
pub fn type_namespace(ty: &str) -> Option<&str> {
    ty.split_once('.').map(|(namespace, _)| namespace)
}

/// Routes each request to the provider of its resource type's namespace,
/// or to the baseline for types in `public` and namespaces without a
/// policy set of their own (ADR-0042).
///
/// Each namespace's provider is built from the baseline's policies and the
/// namespace's together, so the baseline's forbids apply everywhere.
pub struct ScopedProvider {
    /// The provider for `public` and namespaces without a policy set.
    baseline: Arc<dyn PolicyProvider>,
    /// Each namespace's provider, by namespace.
    scopes: BTreeMap<String, Arc<dyn PolicyProvider>>,
}

impl std::fmt::Debug for ScopedProvider {
    /// Render the namespaces with policy sets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedProvider")
            .field("scopes", &self.scopes.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ScopedProvider {
    /// Route to `scopes` by namespace, and to `baseline` otherwise.
    #[must_use]
    pub fn new(
        baseline: Arc<dyn PolicyProvider>,
        scopes: BTreeMap<String, Arc<dyn PolicyProvider>>,
    ) -> Self {
        Self { baseline, scopes }
    }

    /// Return the provider for resources of type `ty`.
    fn route(&self, ty: &str) -> &dyn PolicyProvider {
        type_namespace(ty)
            .and_then(|namespace| self.scopes.get(namespace))
            .map_or(self.baseline.as_ref(), AsRef::as_ref)
    }
}

impl PolicyProvider for ScopedProvider {
    /// Partially evaluate with the resource type's namespace's policies.
    fn partial_eval(
        &self,
        req: &PartialRequest<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Residual, SecurityError> {
        self.route(req.resource_type.as_str())
            .partial_eval(req, entities)
    }

    /// Decide with the resource type's namespace's policies.
    fn authorize(
        &self,
        req: &Request<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        self.route(req.resource.ty.as_str())
            .authorize(req, entities)
    }

    /// Return what the resource type's namespace's policies read.
    fn dependencies(&self, req: &PartialRequest<'_>) -> Dependencies {
        self.route(req.resource_type.as_str()).dependencies(req)
    }

    /// Return the baseline's combining rule, which every namespace shares.
    fn combining_rule(&self) -> CombiningRule {
        self.baseline.combining_rule()
    }

    /// Return every provider's lint findings.
    fn lint(&self) -> Vec<String> {
        let mut out = self.baseline.lint();
        for provider in self.scopes.values() {
            for finding in provider.lint() {
                if !out.contains(&finding) {
                    out.push(finding);
                }
            }
        }
        out
    }

    /// Decide column access with the resource type's namespace's policies.
    fn column_access(
        &self,
        context: &SecurityContext,
        resource_type: &Ident,
        column: &Ident,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        self.route(resource_type.as_str())
            .column_access(context, resource_type, column, entities)
    }

    /// Decide column masks with the resource type's namespace's policies.
    fn column_mask(
        &self,
        context: &SecurityContext,
        resource_type: &Ident,
        column: &Ident,
        entities: &dyn EntityStore,
    ) -> Result<ColumnAccess, SecurityError> {
        self.route(resource_type.as_str())
            .column_mask(context, resource_type, column, entities)
    }

    /// Return the entity paths every provider's policies read.
    fn entity_paths(&self) -> Vec<EntityPath> {
        let mut out = self.baseline.entity_paths();
        for provider in self.scopes.values() {
            for path in provider.entity_paths() {
                if !out.contains(&path) {
                    out.push(path);
                }
            }
        }
        out
    }
}
