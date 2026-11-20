//! The residual cache (ADR-0019).
//!
//! Partial evaluation runs for every governed table in every query, but its
//! result depends only on a few inputs, so it is cached per node. The key
//! holds everything the residual can depend on:
//!
//! - the principal, action and resource type;
//! - the policy version, so a policy change misses;
//! - a fingerprint of the principal's attributes and memberships, when the
//!   policies read them, so a change to the principal's data (a revoked
//!   role) misses;
//! - a fingerprint of every other entity the policies read
//!   (`principal.manager`, entity literals), so a change to one misses;
//! - the values of the context fields the policies read.
//!
//! Residuals that depend on fast-changing context (`time`, `ip`) are not
//! cached. Invalidation needs no protocol: every input that can change is in
//! the key, so stale entries are simply never hit again; the cache is
//! cleared when it reaches its capacity.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::error::SecurityError;
use super::eval::EntityStore;
use super::expr::Policy;
use super::expr::{Action, EntityUid, SecValue};
use super::paths::EntityPath;
use super::provider::{
    ColumnAccess, CombiningRule, Decision, Dependencies, PartialRequest, PolicyProvider, Request,
    Residual, SecurityContext,
};
use crate::names::Ident;

/// Context fields that change too often for residuals depending on them to be cached.
pub const VOLATILE_CONTEXT_FIELDS: [&str; 2] = ["time", "ip"];

/// Everything a residual can depend on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    /// The principal.
    principal: EntityUid,
    /// The action.
    action: Action,
    /// The resource type.
    resource_type: Ident,
    /// The policy version.
    policy_version: i64,
    /// The principal's attributes and ancestors, rendered, if the policies read them.
    principal_data: Option<String>,
    /// The attributes and ancestors of the other entities the policies read, rendered.
    entities: String,
    /// The values of the context fields the policies read, rendered.
    context: Vec<(String, String)>,
}

/// Cache statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups answered from the cache.
    pub hits: u64,
    /// Lookups that computed the residual.
    pub misses: u64,
    /// Lookups not cached because the residual depends on volatile context.
    pub uncacheable: u64,
}

/// A per-node cache of residuals (ADR-0019).
#[derive(Debug)]
pub struct ResidualCache {
    /// Cached residuals.
    entries: Mutex<HashMap<Key, Residual>>,
    /// Maximum entries before the cache is cleared.
    capacity: usize,
    /// Hits.
    hits: AtomicU64,
    /// Misses.
    misses: AtomicU64,
    /// Uncacheable lookups.
    uncacheable: AtomicU64,
}

impl ResidualCache {
    /// Construct a cache holding up to `capacity` residuals.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            uncacheable: AtomicU64::new(0),
        }
    }

    /// Return the hit, miss and uncacheable counts so far.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            uncacheable: self.uncacheable.load(Ordering::Relaxed),
        }
    }

    /// Wrap `inner`, whose policies are at `policy_version`, so its residuals are cached here.
    #[must_use]
    pub fn provider<'a>(
        &'a self,
        inner: &'a dyn PolicyProvider,
        policy_version: i64,
    ) -> CachedProvider<'a> {
        CachedProvider {
            inner,
            cache: self,
            policy_version,
        }
    }
}

/// Render the value at a dotted `path` in the context record, or `None` if absent.
fn context_value(record: &SecValue, path: &str) -> Option<String> {
    let mut value = record;
    for part in path.split('.') {
        value = value.field(part)?;
    }
    Some(format!("{value:?}"))
}

/// A provider whose residuals are cached; everything else passes through.
pub struct CachedProvider<'a> {
    /// The provider whose residuals are cached.
    inner: &'a dyn PolicyProvider,
    /// The cache.
    cache: &'a ResidualCache,
    /// The version of `inner`'s policies.
    policy_version: i64,
}

impl CachedProvider<'_> {
    /// Return the cache key for `req`, or `None` if its residual must not be cached.
    fn key(&self, req: &PartialRequest<'_>, entities: &dyn EntityStore) -> Option<Key> {
        let dependencies = self.inner.dependencies(req);
        let volatile = dependencies
            .context_fields
            .iter()
            .any(|f| VOLATILE_CONTEXT_FIELDS.contains(&f.split('.').next().unwrap_or("")));
        if volatile {
            return None;
        }
        let principal = &req.context.principal;
        let principal_data = dependencies.principal_data.then(|| {
            let mut ancestors = entities.ancestors(principal);
            ancestors.sort();
            format!("{:?} {ancestors:?}", entities.attributes(principal))
        });
        let mut entity_data = String::new();
        for path in self.inner.entity_paths() {
            let uid = path.resolve(principal, entities);
            let data = uid.as_ref().map(|uid| {
                let mut ancestors = entities.ancestors(uid);
                ancestors.sort();
                (entities.attributes(uid), ancestors)
            });
            entity_data.push_str(&format!("{path:?}={uid:?}:{data:?};"));
        }
        let record = req.context.record();
        let context = dependencies
            .context_fields
            .iter()
            .map(|f| (f.clone(), context_value(&record, f).unwrap_or_default()))
            .collect();
        Some(Key {
            principal: principal.clone(),
            action: req.action,
            resource_type: req.resource_type.clone(),
            policy_version: self.policy_version,
            principal_data,
            entities: entity_data,
            context,
        })
    }
}

impl PolicyProvider for CachedProvider<'_> {
    /// Return the cached residual, or compute and cache it.
    fn partial_eval(
        &self,
        req: &PartialRequest<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Residual, SecurityError> {
        let Some(key) = self.key(req, entities) else {
            self.cache.uncacheable.fetch_add(1, Ordering::Relaxed);
            return self.inner.partial_eval(req, entities);
        };
        if let Some(hit) = self.cache.entries.lock().expect("cache lock").get(&key) {
            self.cache.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit.clone());
        }
        self.cache.misses.fetch_add(1, Ordering::Relaxed);
        let residual = self.inner.partial_eval(req, entities)?;
        let mut entries = self.cache.entries.lock().expect("cache lock");
        if entries.len() >= self.cache.capacity {
            entries.clear();
        }
        entries.insert(key, residual.clone());
        Ok(residual)
    }

    /// Pass through: writes are decided concretely.
    fn authorize(
        &self,
        req: &Request<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        self.inner.authorize(req, entities)
    }

    /// Pass through.
    fn dependencies(&self, req: &PartialRequest<'_>) -> Dependencies {
        self.inner.dependencies(req)
    }

    /// Pass through.
    fn combining_rule(&self) -> CombiningRule {
        self.inner.combining_rule()
    }

    /// Pass through: column decisions are cheap.
    fn column_access(
        &self,
        context: &SecurityContext,
        resource_type: &Ident,
        column: &Ident,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        self.inner
            .column_access(context, resource_type, column, entities)
    }

    /// Pass through: masks are decided per query.
    fn column_mask(
        &self,
        context: &SecurityContext,
        resource_type: &Ident,
        column: &Ident,
        entities: &dyn EntityStore,
    ) -> Result<ColumnAccess, SecurityError> {
        self.inner
            .column_mask(context, resource_type, column, entities)
    }

    /// Pass through.
    fn entity_paths(&self) -> Vec<EntityPath> {
        self.inner.entity_paths()
    }

    /// Pass through.
    fn policies(&self) -> Option<&[Policy]> {
        self.inner.policies()
    }
}
