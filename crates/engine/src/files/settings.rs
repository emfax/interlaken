//! Which namespaces use files, and how (ADR-0044).

use interlaken_storage::catalog::{self, ColumnDef, TableDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::{Txn, rows};

use super::stores::FileStore;
use crate::EngineError;

/// The table of namespaces with files, one row each.
pub(crate) const FILE_NAMESPACES: &str = "engine_fs_namespaces";
/// The one-row table holding the cluster's default file store.
pub(crate) const FILE_DEFAULTS: &str = "engine_fs_defaults";

/// How a namespace's files are kept, set by `Session::enable_files`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileOptions {
    /// The entity type of the principals who own files, such as `User`, so
    /// policies can compare `resource.owner` with `principal`. A file
    /// created by a principal of another type has no `owner` attribute.
    /// `None` declares no `owner` attribute.
    pub owner_type: Option<String>,
    /// The most bytes of file content the namespace may hold, counting
    /// each file's size; `None` for no limit.
    pub quota_bytes: Option<u64>,
    /// The most files and directories the namespace may hold, the root
    /// aside; `None` for no limit.
    pub quota_files: Option<u64>,
    /// Where the namespace's file content is kept: the cluster's default
    /// store unless this says otherwise. It can't be changed later.
    pub store: FileStore,
    /// For an object store, the prefix of the namespace's object keys; the
    /// namespace's name if `None`.
    pub prefix: Option<String>,
}

/// What a namespace's files use, and may use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileUsage {
    /// Bytes of file content: the sum of file sizes.
    pub bytes: u64,
    /// Files and directories, the root aside.
    pub files: u64,
    /// The byte quota, if any.
    pub quota_bytes: Option<u64>,
    /// The file quota, if any.
    pub quota_files: Option<u64>,
}

/// A namespace's file settings, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    /// The namespace.
    pub(crate) namespace: String,
    /// The stored name of the owner entity type, if any.
    pub(crate) owner_type: Option<String>,
    /// The IR type of the owner type's key, such as `int8`.
    pub(crate) owner_key_type: Option<String>,
    /// The byte quota, if any.
    pub(crate) quota_bytes: Option<u64>,
    /// The file quota, if any.
    pub(crate) quota_files: Option<u64>,
    /// The object store holding the namespace's content, and its key
    /// prefix; `None` for the database.
    pub(crate) object: Option<(String, String)>,
}

/// Return the settings table, creating it if this database has none yet.
pub(crate) async fn settings_table(txn: &Txn) -> Result<TableDef, EngineError> {
    if let Some(table) = catalog::table(txn, FILE_NAMESPACES).await? {
        return Ok(table);
    }
    Ok(catalog::create_table(
        txn,
        TableSpec {
            name: FILE_NAMESPACES.into(),
            columns: vec![
                ColumnDef::new("namespace", ColumnType::Text).not_null(),
                ColumnDef::new("owner_type", ColumnType::Text),
                ColumnDef::new("owner_key_type", ColumnType::Text),
                ColumnDef::new("quota_bytes", ColumnType::Int8),
                ColumnDef::new("quota_files", ColumnType::Int8),
                ColumnDef::new("used_bytes", ColumnType::Int8).not_null(),
                ColumnDef::new("used_files", ColumnType::Int8).not_null(),
                ColumnDef::new("store", ColumnType::Text),
                ColumnDef::new("prefix", ColumnType::Text),
            ],
            primary_key: vec!["namespace".into()],
        },
    )
    .await?)
}

/// Return the optional text at `i` of `row`.
fn optional_text(row: &[Datum], i: usize) -> Option<String> {
    match row.get(i) {
        Some(Datum::Text(t)) => Some(t.clone()),
        _ => None,
    }
}

/// Return the optional count at `i` of `row`.
fn optional_count(row: &[Datum], i: usize) -> Option<u64> {
    match row.get(i) {
        Some(Datum::Int8(n)) => u64::try_from(*n).ok(),
        _ => None,
    }
}

/// Return a count as stored.
fn count_datum(n: u64) -> Datum {
    Datum::Int8(i64::try_from(n).unwrap_or(i64::MAX))
}

/// Return the file settings of `namespace`, or `None` if it has no files.
///
/// # Errors
///
/// Returns a storage error.
pub(crate) async fn load(txn: &Txn, namespace: &str) -> Result<Option<Settings>, EngineError> {
    let Some(table) = catalog::table(txn, FILE_NAMESPACES).await? else {
        return Ok(None);
    };
    Ok(rows::get(txn, &table, &[Datum::Text(namespace.to_owned())])
        .await?
        .map(|row| Settings {
            namespace: namespace.to_owned(),
            owner_type: optional_text(&row, 1),
            owner_key_type: optional_text(&row, 2),
            quota_bytes: optional_count(&row, 3),
            quota_files: optional_count(&row, 4),
            object: optional_text(&row, 7).zip(optional_text(&row, 8)),
        }))
}

/// Record `settings`.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if the namespace has files
/// already, or a storage error.
pub(crate) async fn store(txn: &Txn, settings: &Settings) -> Result<(), EngineError> {
    let table = settings_table(txn).await?;
    let text = |t: &Option<String>| t.clone().map_or(Datum::Null, Datum::Text);
    let key = [Datum::Text(settings.namespace.clone())];
    if rows::get(txn, &table, &key).await?.is_some() {
        return Err(EngineError::InvalidDefinition(format!(
            "namespace {} has files already",
            settings.namespace
        )));
    }
    rows::insert(
        txn,
        &table,
        vec![
            Datum::Text(settings.namespace.clone()),
            text(&settings.owner_type),
            text(&settings.owner_key_type),
            settings.quota_bytes.map_or(Datum::Null, count_datum),
            settings.quota_files.map_or(Datum::Null, count_datum),
            Datum::Int8(0),
            Datum::Int8(0),
            text(&settings.object.as_ref().map(|(store, _)| store.clone())),
            text(&settings.object.as_ref().map(|(_, prefix)| prefix.clone())),
        ],
    )
    .await?;
    Ok(())
}

/// Forget `namespace`'s file settings, when it is dropped.
///
/// # Errors
///
/// Returns a storage error.
pub(crate) async fn forget(txn: &Txn, namespace: &str) -> Result<(), EngineError> {
    if let Some(table) = catalog::table(txn, FILE_NAMESPACES).await? {
        rows::delete(txn, &table, &[Datum::Text(namespace.to_owned())]).await?;
    }
    Ok(())
}

/// Return what `namespace`'s files use, and may use.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if the namespace has no
/// files, or a storage error.
pub(crate) async fn usage(txn: &Txn, namespace: &str) -> Result<FileUsage, EngineError> {
    let table = settings_table(txn).await?;
    let row = rows::get(txn, &table, &[Datum::Text(namespace.to_owned())])
        .await?
        .ok_or_else(|| {
            EngineError::InvalidDefinition(format!("namespace {namespace} has no files"))
        })?;
    Ok(FileUsage {
        quota_bytes: optional_count(&row, 3),
        quota_files: optional_count(&row, 4),
        bytes: optional_count(&row, 5).unwrap_or(0),
        files: optional_count(&row, 6).unwrap_or(0),
    })
}

/// Change `namespace`'s usage by `files` and `bytes`, in the caller's
/// transaction, so it stays exact; concurrent writers conflict on it.
///
/// # Errors
///
/// Returns [`EngineError::QuotaExceeded`] if an increase passes a quota,
/// or a storage error.
pub(crate) async fn adjust_usage(
    txn: &Txn,
    namespace: &str,
    files: i64,
    bytes: i64,
) -> Result<(), EngineError> {
    if files == 0 && bytes == 0 {
        return Ok(());
    }
    let table = settings_table(txn).await?;
    let key = [Datum::Text(namespace.to_owned())];
    let mut row = rows::get(txn, &table, &key).await?.ok_or_else(|| {
        EngineError::InvalidDefinition(format!("namespace {namespace} has no files"))
    })?;
    let used_bytes = optional_count(&row, 5)
        .unwrap_or(0)
        .saturating_add_signed(bytes);
    let used_files = optional_count(&row, 6)
        .unwrap_or(0)
        .saturating_add_signed(files);
    if let Some(quota) = optional_count(&row, 3)
        && bytes > 0
        && used_bytes > quota
    {
        return Err(EngineError::QuotaExceeded(format!(
            "namespace {namespace} would hold {used_bytes} bytes of files, over its quota of {quota}"
        )));
    }
    if let Some(quota) = optional_count(&row, 4)
        && files > 0
        && used_files > quota
    {
        return Err(EngineError::QuotaExceeded(format!(
            "namespace {namespace} would hold {used_files} files, over its quota of {quota}"
        )));
    }
    row[5] = count_datum(used_bytes);
    row[6] = count_datum(used_files);
    rows::update(txn, &table, &key, row).await?;
    Ok(())
}

/// Set `namespace`'s quotas.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if the namespace has no
/// files, or a storage error.
pub(crate) async fn set_quota(
    txn: &Txn,
    namespace: &str,
    bytes: Option<u64>,
    files: Option<u64>,
) -> Result<(), EngineError> {
    let table = settings_table(txn).await?;
    let key = [Datum::Text(namespace.to_owned())];
    let mut row = rows::get(txn, &table, &key).await?.ok_or_else(|| {
        EngineError::InvalidDefinition(format!("namespace {namespace} has no files"))
    })?;
    row[3] = bytes.map_or(Datum::Null, count_datum);
    row[4] = files.map_or(Datum::Null, count_datum);
    rows::update(txn, &table, &key, row).await?;
    Ok(())
}

/// Return the defaults table, creating it if this database has none yet.
async fn defaults_table(txn: &Txn) -> Result<TableDef, EngineError> {
    if let Some(table) = catalog::table(txn, FILE_DEFAULTS).await? {
        return Ok(table);
    }
    Ok(catalog::create_table(
        txn,
        TableSpec {
            name: FILE_DEFAULTS.into(),
            columns: vec![
                ColumnDef::new("id", ColumnType::Int8).not_null(),
                ColumnDef::new("store", ColumnType::Text),
            ],
            primary_key: vec!["id".into()],
        },
    )
    .await?)
}

/// Return the cluster's default file store: an object store's name, or
/// `None` for the database.
///
/// # Errors
///
/// Returns a storage error.
pub(crate) async fn default_store(txn: &Txn) -> Result<Option<String>, EngineError> {
    let Some(table) = catalog::table(txn, FILE_DEFAULTS).await? else {
        return Ok(None);
    };
    Ok(rows::get(txn, &table, &[Datum::Int8(1)])
        .await?
        .and_then(|row| optional_text(&row, 1)))
}

/// Set the cluster's default file store.
///
/// # Errors
///
/// Returns a storage error.
pub(crate) async fn set_default_store(txn: &Txn, store: Option<&str>) -> Result<(), EngineError> {
    let table = defaults_table(txn).await?;
    let row = vec![
        Datum::Int8(1),
        store.map_or(Datum::Null, |s| Datum::Text(s.to_owned())),
    ];
    if !rows::update(txn, &table, &[Datum::Int8(1)], row.clone()).await? {
        rows::insert(txn, &table, row).await?;
    }
    Ok(())
}
