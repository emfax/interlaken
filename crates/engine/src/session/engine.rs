//! The engine: one database, shared by its sessions (ADR-0031).

use std::path::Path;
use std::sync::{Arc, Mutex};

use interlaken_exec::{DEFAULT_QUERY_BUDGET, MemoryPool};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::security::{
    CacheStats, EntityUid, PolicyProvider, ResidualCache, RlsProvider, SecurityContext,
};
use interlaken_storage::{FjallStorage, Storage, StorageOptions, TxnOptions};

use super::connection::Session;
use crate::catalog::{CatalogCache, create_catalog_tables, schema_version};
use crate::error::EngineError;
use crate::policy::{
    ActivePolicies, PolicyDocument, PolicySettings, VerificationMode, build_provider,
    create_policy_tables,
};
use crate::privileges::{Privilege, grant};

/// Settings for [`Engine::create`] and [`Engine::open`].
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Storage settings.
    pub storage: StorageOptions,
    /// Each statement's memory budget in bytes (ADR-0027).
    pub query_budget: usize,
    /// A node-wide cap on query memory shared by all sessions, if any.
    pub node_memory_limit: Option<usize>,
    /// How many residuals the residual cache holds (ADR-0019).
    pub residual_cache_capacity: usize,
    /// The policy settings a new database starts with ([`Engine::create`] only).
    pub policy_settings: PolicySettings,
}

impl Default for EngineOptions {
    /// Return default storage settings, the default query budget, no node
    /// cap, a 4096-entry residual cache and analysis-sufficient policy
    /// verification with an empty application context.
    fn default() -> Self {
        Self {
            storage: StorageOptions::default(),
            query_budget: DEFAULT_QUERY_BUDGET,
            node_memory_limit: None,
            residual_cache_capacity: 4096,
            policy_settings: PolicySettings {
                mode: VerificationMode::AnalysisSufficient,
                app_context: "{}".into(),
            },
        }
    }
}

/// A policy provider built for one catalog and policy set.
struct ProviderEntry {
    /// The catalog's schema version.
    schema_version: u64,
    /// The policies' version.
    policy_version: i64,
    /// The policies.
    document: PolicyDocument,
    /// The application context type the provider validated against.
    app_context: String,
    /// The provider.
    provider: Arc<dyn PolicyProvider>,
}

/// What the engine's sessions share.
pub(crate) struct Inner {
    /// The storage.
    pub(crate) storage: FjallStorage,
    /// Loaded catalogs by schema version.
    pub(crate) catalogs: CatalogCache,
    /// The provider for the newest committed catalog and policies.
    providers: Mutex<Option<ProviderEntry>>,
    /// Residuals by everything they depend on (ADR-0019).
    pub(crate) residuals: ResidualCache,
    /// The node-wide memory pool, if any.
    pub(crate) pool: Option<Arc<MemoryPool>>,
    /// Each statement's memory budget.
    pub(crate) query_budget: usize,
}

impl Inner {
    /// Return the policy provider for `policies` over `catalog`, reusing
    /// the last one built if `use_cache` and nothing it depends on changed.
    pub(crate) fn provider(
        &self,
        catalog: &Arc<InMemoryCatalog>,
        policies: &ActivePolicies,
        app_context: &str,
        use_cache: bool,
    ) -> Result<Arc<dyn PolicyProvider>, EngineError> {
        let version = catalog.schema_version().get();
        if catalog.entities().is_empty() {
            // Nothing is governed, so no policy applies; a Cedar schema
            // needs at least one entity type.
            return Ok(Arc::new(RlsProvider::new(Vec::new())));
        }
        if use_cache
            && let Some(entry) = self.providers.lock().expect("provider lock").as_ref()
            && entry.schema_version == version
            && entry.policy_version == policies.version
            && entry.document == policies.document
            && entry.app_context == app_context
        {
            return Ok(entry.provider.clone());
        }
        let shared: Arc<dyn Catalog> = catalog.clone();
        let provider: Arc<dyn PolicyProvider> =
            Arc::from(build_provider(&policies.document, shared, app_context)?);
        if use_cache {
            *self.providers.lock().expect("provider lock") = Some(ProviderEntry {
                schema_version: version,
                policy_version: policies.version,
                document: policies.document.clone(),
                app_context: app_context.to_owned(),
                provider: provider.clone(),
            });
        }
        Ok(provider)
    }
}

/// An embedded database: storage, its catalog and policies, and the caches
/// its sessions share.
///
/// Cloning is cheap; clones share everything.
#[derive(Clone)]
pub struct Engine {
    /// The shared state.
    pub(crate) inner: Arc<Inner>,
}

impl Engine {
    /// Wrap opened storage.
    fn new(storage: FjallStorage, options: &EngineOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                storage,
                catalogs: CatalogCache::default(),
                providers: Mutex::new(None),
                residuals: ResidualCache::new(options.residual_cache_capacity),
                pool: options.node_memory_limit.map(MemoryPool::new),
                query_budget: options.query_budget,
            }),
        }
    }

    /// Create a database at `path`, with `admin` as its first administrator.
    ///
    /// The database starts with no tables and an empty Cedar policy set,
    /// which denies everything.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Catalog`] if `path` already holds a database,
    /// or a storage error.
    pub async fn create(
        path: impl AsRef<Path>,
        options: EngineOptions,
        admin: &EntityUid,
    ) -> Result<Self, EngineError> {
        let storage = FjallStorage::open(path, options.storage.clone())?;
        let txn = storage.begin(TxnOptions::default()).await?;
        if interlaken_storage::catalog::tables(&txn)
            .await?
            .iter()
            .any(|t| crate::catalog::is_engine_table(t.name()))
        {
            txn.abort();
            return Err(EngineError::Catalog("the database already exists".into()));
        }
        create_catalog_tables(&txn).await?;
        create_policy_tables(&txn, &options.policy_settings).await?;
        grant(&txn, admin, Privilege::Admin).await?;
        txn.commit().await?;
        Ok(Self::new(storage, &options))
    }

    /// Open the database at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Catalog`] if `path` holds no database created
    /// by [`Engine::create`], or a storage error.
    pub async fn open(path: impl AsRef<Path>, options: EngineOptions) -> Result<Self, EngineError> {
        let storage = FjallStorage::open(path, options.storage.clone())?;
        let txn = storage
            .begin(TxnOptions {
                read_only: true,
                ..TxnOptions::default()
            })
            .await?;
        let found = schema_version(&txn).await;
        txn.abort();
        found.map_err(|_| EngineError::Catalog("no database was created here".into()))?;
        Ok(Self::new(storage, &options))
    }

    /// Start a session for the principal and request facts in `context`.
    ///
    /// The caller authenticates the principal; the engine trusts `context`.
    #[must_use]
    pub fn session(&self, context: SecurityContext) -> Session {
        Session::new(self.clone(), context)
    }

    /// Return the underlying storage.
    #[must_use]
    pub fn storage(&self) -> &FjallStorage {
        &self.inner.storage
    }

    /// Return the node-wide memory pool, if [`EngineOptions::node_memory_limit`] set one.
    ///
    /// Its [`MemoryPool::used`] and [`MemoryPool::peak`] show what running
    /// statements hold.
    #[must_use]
    pub fn memory_pool(&self) -> Option<&Arc<MemoryPool>> {
        self.inner.pool.as_ref()
    }

    /// Return the residual cache's hit, miss and uncacheable counts.
    #[must_use]
    pub fn residual_cache_stats(&self) -> CacheStats {
        self.inner.residuals.stats()
    }
}
