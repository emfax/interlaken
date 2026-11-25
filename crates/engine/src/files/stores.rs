//! Object stores that can hold file content instead of the database
//! (ADR-0044 phase 3).
//!
//! A node is configured with named stores. The database records only a
//! store's name, for each namespace and as the cluster's default, never
//! where it is or how to reach it: credentials come from the node's
//! environment, so no secret is ever replicated, logged or snapshotted.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "object-store")]
use std::sync::{Arc, PoisonError};

#[cfg(feature = "object-store")]
use object_store::ObjectStoreExt;

use crate::EngineError;

/// Where an object store is, on one node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectStoreLocation {
    /// An S3-compatible service: AWS S3, MinIO, Ceph, R2 and the like.
    S3 {
        /// The bucket.
        bucket: String,
        /// The endpoint URL, such as `https://minio.internal:9000`; `None`
        /// for AWS S3 itself.
        endpoint: Option<String>,
        /// The region; `None` to take it from the environment.
        region: Option<String>,
        /// Allow a plain `http://` endpoint. Only for local testing: every
        /// other endpoint must use TLS.
        allow_http: bool,
        /// Address buckets by path (`endpoint/bucket/key`) rather than by
        /// host name, as MinIO and most self-hosted services need.
        path_style: bool,
    },
    /// A directory, such as a mounted network file system, shared by every
    /// node that uses the store.
    Directory(std::path::PathBuf),
}

/// A named object store a node can reach.
///
/// Credentials for S3 come from the node's environment, by the standard
/// AWS chain: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, a web
/// identity token file (`AWS_WEB_IDENTITY_TOKEN_FILE` with
/// `AWS_ROLE_ARN`), container credentials, or the instance's role. There
/// is deliberately no way to give keys here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectStoreConfig {
    /// The name namespaces refer to it by.
    pub name: String,
    /// Where it is.
    pub location: ObjectStoreLocation,
}

impl ObjectStoreConfig {
    /// Construct the configuration of an S3 bucket named `name`, on AWS
    /// unless [`ObjectStoreConfig::endpoint`] says otherwise.
    #[must_use]
    pub fn s3(name: impl Into<String>, bucket: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            location: ObjectStoreLocation::S3 {
                bucket: bucket.into(),
                endpoint: None,
                region: None,
                allow_http: false,
                path_style: false,
            },
        }
    }

    /// Construct the configuration of a directory store named `name`.
    #[must_use]
    pub fn directory(name: impl Into<String>, path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            name: name.into(),
            location: ObjectStoreLocation::Directory(path.into()),
        }
    }

    /// Return a copy using `endpoint`, with path-style addressing, as an
    /// S3-compatible service other than AWS needs.
    #[must_use]
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        if let ObjectStoreLocation::S3 {
            endpoint: e,
            path_style,
            ..
        } = &mut self.location
        {
            *e = Some(endpoint.into());
            *path_style = true;
        }
        self
    }

    /// Return a copy in `region`.
    #[must_use]
    pub fn region(mut self, region: impl Into<String>) -> Self {
        if let ObjectStoreLocation::S3 { region: r, .. } = &mut self.location {
            *r = Some(region.into());
        }
        self
    }

    /// Return a copy that allows a plain `http://` endpoint, for local
    /// testing only.
    #[must_use]
    pub fn allow_http(mut self) -> Self {
        if let ObjectStoreLocation::S3 { allow_http, .. } = &mut self.location {
            *allow_http = true;
        }
        self
    }
}

/// Which store a namespace keeps its file content in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FileStore {
    /// The cluster's default store, or the database if it has none.
    #[default]
    Default,
    /// The database, replicated by Raft.
    Database,
    /// The object store of this name, which every node must be configured
    /// with.
    Object(String),
}

/// A connected object store.
#[cfg(feature = "object-store")]
type Client = Arc<dyn object_store::ObjectStore>;

/// The object stores this node is configured with, connected on first use.
pub(crate) struct FileStores {
    /// The configured stores, by name.
    configs: HashMap<String, ObjectStoreConfig>,
    /// The stores connected so far.
    #[cfg(feature = "object-store")]
    clients: Mutex<HashMap<String, Client>>,
    /// Unused without object store support.
    #[cfg(not(feature = "object-store"))]
    clients: Mutex<()>,
}

impl std::fmt::Debug for FileStores {
    /// Show the configured stores' names.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileStores")
            .field("stores", &self.configs.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Return a key no other staging of the same chunk uses: the chunk's
/// hash with a random suffix, under `prefix`. Each staging writes its own
/// object, so collecting one chunk's object never races with staging it
/// again.
pub(crate) fn object_key(prefix: &str, hash: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    hasher.write_u32(std::process::id());
    format!("{prefix}/{hash}-{:016x}", hasher.finish())
}

impl FileStores {
    /// Construct the registry of `configs`.
    pub(crate) fn new(configs: &[ObjectStoreConfig]) -> Self {
        Self {
            configs: configs
                .iter()
                .map(|c| (c.name.clone(), c.clone()))
                .collect(),
            clients: Mutex::default(),
        }
    }

    /// Return `true` if this node is configured with store `name`.
    pub(crate) fn has(&self, name: &str) -> bool {
        self.configs.contains_key(name)
    }

    /// Return the error for a store this node lacks.
    fn missing(name: &str) -> EngineError {
        EngineError::InvalidDefinition(format!("object store {name} isn't configured on this node"))
    }

    /// Return store `name`, connecting it on first use.
    #[cfg(feature = "object-store")]
    fn client(&self, name: &str) -> Result<Client, EngineError> {
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(client) = clients.get(name) {
            return Ok(Arc::clone(client));
        }
        let config = self.configs.get(name).ok_or_else(|| Self::missing(name))?;
        let client: Client = match &config.location {
            ObjectStoreLocation::S3 {
                bucket,
                endpoint,
                region,
                allow_http,
                path_style,
            } => {
                let mut builder = object_store::aws::AmazonS3Builder::from_env()
                    .with_bucket_name(bucket)
                    .with_allow_http(*allow_http)
                    .with_virtual_hosted_style_request(!path_style);
                if let Some(endpoint) = endpoint {
                    builder = builder.with_endpoint(endpoint);
                }
                if let Some(region) = region {
                    builder = builder.with_region(region);
                }
                Arc::new(builder.build().map_err(|e| store_error(name, &e))?)
            }
            ObjectStoreLocation::Directory(path) => {
                std::fs::create_dir_all(path).map_err(|e| {
                    EngineError::InvalidDefinition(format!("object store {name}: {e}"))
                })?;
                Arc::new(
                    object_store::local::LocalFileSystem::new_with_prefix(path)
                        .map_err(|e| store_error(name, &e))?,
                )
            }
        };
        clients.insert(name.to_owned(), Arc::clone(&client));
        Ok(client)
    }

    /// Store `bytes` as object `key` of store `name`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::ObjectStore`] if the upload fails.
    #[cfg(feature = "object-store")]
    pub(crate) async fn put(
        &self,
        name: &str,
        key: &str,
        bytes: Vec<u8>,
    ) -> Result<(), EngineError> {
        let client = self.client(name)?;
        client
            .put(&key.into(), bytes.into())
            .await
            .map_err(|e| store_error(name, &e))?;
        Ok(())
    }

    /// Return object `key` of store `name`, or `None` if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::ObjectStore`] if the download fails.
    #[cfg(feature = "object-store")]
    pub(crate) async fn get(&self, name: &str, key: &str) -> Result<Option<Vec<u8>>, EngineError> {
        let client = self.client(name)?;
        match client.get(&key.into()).await {
            Ok(result) => Ok(Some(
                result
                    .bytes()
                    .await
                    .map_err(|e| store_error(name, &e))?
                    .to_vec(),
            )),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(store_error(name, &e)),
        }
    }

    /// Delete object `key` of store `name`; one that is gone already is
    /// no error.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::ObjectStore`] if the deletion fails.
    #[cfg(feature = "object-store")]
    pub(crate) async fn delete(&self, name: &str, key: &str) -> Result<(), EngineError> {
        let client = self.client(name)?;
        match client.delete(&key.into()).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(store_error(name, &e)),
        }
    }

    /// Fail: this build has no object store support.
    #[cfg(not(feature = "object-store"))]
    pub(crate) async fn put(
        &self,
        name: &str,
        _key: &str,
        _bytes: Vec<u8>,
    ) -> Result<(), EngineError> {
        Err(self.unsupported(name))
    }

    /// Fail: this build has no object store support.
    #[cfg(not(feature = "object-store"))]
    pub(crate) async fn get(&self, name: &str, _key: &str) -> Result<Option<Vec<u8>>, EngineError> {
        Err(self.unsupported(name))
    }

    /// Fail: this build has no object store support.
    #[cfg(not(feature = "object-store"))]
    pub(crate) async fn delete(&self, name: &str, _key: &str) -> Result<(), EngineError> {
        Err(self.unsupported(name))
    }

    /// Return the error for using a store in a build without support.
    #[cfg(not(feature = "object-store"))]
    fn unsupported(&self, name: &str) -> EngineError {
        let _ = &self.clients;
        if !self.has(name) {
            return Self::missing(name);
        }
        EngineError::NotSupported(format!(
            "object store {name}: this build has no object store support (the `object-store` feature)"
        ))
    }
}

/// Return an object store failure as an engine error. The store's own
/// message never holds credentials.
#[cfg(feature = "object-store")]
fn store_error(name: &str, error: &object_store::Error) -> EngineError {
    EngineError::ObjectStore(format!("object store {name}: {error}"))
}
