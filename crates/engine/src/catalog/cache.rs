//! Caching loaded catalogs by schema version (ADR-0031).
//!
//! Loading the catalog reads every catalog table, so the engine keeps the
//! catalogs of the last few committed schema versions. A transaction reads
//! the schema version row through itself (a dependency), then takes the
//! cached catalog at that version, or loads and caches it. Every DDL
//! statement bumps the version and concurrent DDL conflicts on that row,
//! so committed versions name exactly one catalog each. A transaction that
//! ran DDL itself sees an uncommitted version, which another transaction
//! may also have reached with different DDL; it neither reads nor fills
//! the cache.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_storage::Txn;

use super::load::load_catalog;
use super::tables::schema_version;
use crate::error::EngineError;

/// How many schema versions' catalogs to keep.
const VERSIONS: usize = 4;

/// The catalogs of recent committed schema versions.
#[derive(Debug, Default)]
pub(crate) struct CatalogCache {
    /// Newest last.
    entries: Mutex<VecDeque<Arc<InMemoryCatalog>>>,
}

impl CatalogCache {
    /// Return the catalog as `txn` sees it; `dirty` if `txn` has run DDL.
    pub(crate) async fn catalog(
        &self,
        txn: &Txn,
        dirty: bool,
    ) -> Result<Arc<InMemoryCatalog>, EngineError> {
        if dirty {
            return Ok(Arc::new(load_catalog(txn).await?));
        }
        let version = schema_version(txn).await?;
        let cached = self
            .entries
            .lock()
            .expect("catalog cache lock")
            .iter()
            .find(|c| c.schema_version().get() == version)
            .cloned();
        if let Some(catalog) = cached {
            return Ok(catalog);
        }
        let catalog = Arc::new(load_catalog(txn).await?);
        let mut entries = self.entries.lock().expect("catalog cache lock");
        if !entries
            .iter()
            .any(|c| c.schema_version() == catalog.schema_version())
        {
            if entries.len() == VERSIONS {
                entries.pop_front();
            }
            entries.push_back(catalog.clone());
        }
        Ok(catalog)
    }
}
