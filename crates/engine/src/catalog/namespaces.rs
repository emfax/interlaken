//! Namespaces: one level of names, each with an owner (ADR-0042).

use interlaken_ir::security::{EntityKey, EntityUid};
use interlaken_storage::catalog::{self, ColumnDef, TableDef};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::{Txn, rows};

use super::scoped::{PUBLIC, split_name, stored_name};
use super::tables::{
    ENTITIES, FOREIGN_KEYS, MEMBERSHIPS, PRINCIPAL_TYPES, bump_schema_version, read_all, text_at,
};
use crate::EngineError;

/// The table of namespaces and their owners.
pub(crate) const NAMESPACES: &str = "engine_namespaces";

/// Return the error for an invalid request.
fn invalid(reason: String) -> EngineError {
    EngineError::InvalidDefinition(reason)
}

/// Return the stored form of a principal: its type and its key as text.
fn principal_columns(principal: &EntityUid) -> [Datum; 2] {
    let key = match &principal.key {
        EntityKey::Long(v) => format!("long:{v}"),
        EntityKey::Str(s) => format!("str:{s}"),
    };
    [Datum::Text(principal.ty.to_string()), Datum::Text(key)]
}

/// Return the namespaces table, creating it in a database made before
/// ADR-0042, whose `public` namespace then has no owner.
async fn table(txn: &Txn) -> Result<TableDef, EngineError> {
    if let Some(table) = catalog::table(txn, NAMESPACES).await? {
        return Ok(table);
    }
    let text = |n: &str| ColumnDef::new(n, ColumnType::Text).not_null();
    Ok(catalog::create_table(
        txn,
        catalog::TableSpec {
            name: NAMESPACES.into(),
            columns: vec![text("name"), text("owner_type"), text("owner_key")],
            primary_key: vec!["name".into()],
        },
    )
    .await?)
}

/// Create the namespaces table with `public`, owned by `owner`.
pub(crate) async fn create_namespaces_table(
    txn: &Txn,
    owner: &EntityUid,
) -> Result<(), EngineError> {
    let table = table(txn).await?;
    let [ty, key] = principal_columns(owner);
    rows::insert(txn, &table, vec![Datum::Text(PUBLIC.into()), ty, key]).await?;
    Ok(())
}

/// Check that `name` can name a namespace.
fn check_name(name: &str) -> Result<(), EngineError> {
    let valid = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && !name.starts_with("engine_")
        && !name.starts_with("sys_");
    if valid {
        Ok(())
    } else {
        Err(invalid(format!("{name:?} isn't a namespace name")))
    }
}

/// Return `true` if namespace `name` exists. `public` always does.
///
/// # Errors
///
/// Returns a storage error.
pub async fn namespace_exists(txn: &Txn, name: &str) -> Result<bool, EngineError> {
    if name == PUBLIC {
        return Ok(true);
    }
    let Some(table) = catalog::table(txn, NAMESPACES).await? else {
        return Ok(false);
    };
    Ok(rows::get(txn, &table, &[Datum::Text(name.to_owned())])
        .await?
        .is_some())
}

/// Return `true` if `principal` owns namespace `name`.
///
/// # Errors
///
/// Returns a storage error.
pub async fn owns(txn: &Txn, principal: &EntityUid, name: &str) -> Result<bool, EngineError> {
    let Some(table) = catalog::table(txn, NAMESPACES).await? else {
        return Ok(false);
    };
    let Some(row) = rows::get(txn, &table, &[Datum::Text(name.to_owned())]).await? else {
        return Ok(false);
    };
    Ok(row[1..] == principal_columns(principal))
}

/// Create namespace `name`, owned by `owner` (ADR-0042).
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if the name is taken or
/// isn't an identifier.
pub async fn create_namespace(txn: &Txn, name: &str, owner: &EntityUid) -> Result<(), EngineError> {
    check_name(name)?;
    if name == PUBLIC || namespace_exists(txn, name).await? {
        return Err(invalid(format!("namespace {name} already exists")));
    }
    let table = table(txn).await?;
    let [ty, key] = principal_columns(owner);
    rows::insert(txn, &table, vec![Datum::Text(name.to_owned()), ty, key]).await?;
    bump_schema_version(txn).await?;
    Ok(())
}

/// Give namespace `name` to `owner`.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if there is no such namespace.
pub async fn set_namespace_owner(
    txn: &Txn,
    name: &str,
    owner: &EntityUid,
) -> Result<(), EngineError> {
    let table = table(txn).await?;
    let [ty, key] = principal_columns(owner);
    let row = vec![Datum::Text(name.to_owned()), ty, key];
    let replaced = rows::update(txn, &table, &[Datum::Text(name.to_owned())], row.clone()).await?;
    if !replaced {
        if name != PUBLIC {
            return Err(invalid(format!("namespace {name} does not exist")));
        }
        // `public` in a database made before ADR-0042.
        rows::insert(txn, &table, row).await?;
    }
    bump_schema_version(txn).await?;
    Ok(())
}

/// Return `true` if stored name `stored` is in namespace `namespace`.
fn within(stored: &str, namespace: &str) -> bool {
    split_name(stored).0 == namespace
}

/// Drop namespace `name`. Without `cascade` it must be empty; with it,
/// everything in it goes first: foreign keys (including those of other
/// namespaces' tables referring to its tables), memberships, entity and
/// principal types, then tables with their indexes.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if there is no such
/// namespace, it is `public`, or it isn't empty and `cascade` is unset.
pub async fn drop_namespace(txn: &Txn, name: &str, cascade: bool) -> Result<(), EngineError> {
    if name == PUBLIC {
        return Err(invalid("namespace public can't be dropped".into()));
    }
    if !namespace_exists(txn, name).await? {
        return Err(invalid(format!("namespace {name} does not exist")));
    }
    let tables: Vec<TableDef> = catalog::tables(txn)
        .await?
        .into_iter()
        .filter(|t| within(t.name(), name))
        .collect();
    let table_ids: Vec<i32> = tables.iter().map(|t| t.id().get()).collect();
    let mut foreign_keys = Vec::new();
    for row in read_all(txn, FOREIGN_KEYS, Vec::new()).await? {
        let fk = text_at(&row, 0)?.to_owned();
        let of = |i: usize| match row.get(i) {
            Some(Datum::Int4(id)) => table_ids.contains(id),
            _ => false,
        };
        if within(&fk, name) || of(1) || of(2) {
            foreign_keys.push(fk);
        }
    }
    let mut memberships = Vec::new();
    for row in read_all(txn, MEMBERSHIPS, Vec::new()).await? {
        let (member, group) = (text_at(&row, 0)?, text_at(&row, 1)?);
        if within(member, name) || within(group, name) {
            memberships.push((member.to_owned(), group.to_owned()));
        }
    }
    let mut entities = Vec::new();
    for row in read_all(txn, ENTITIES, Vec::new()).await? {
        let entity = text_at(&row, 0)?;
        if within(entity, name) {
            entities.push(entity.to_owned());
        }
    }
    let mut principal_types = Vec::new();
    if catalog::table(txn, PRINCIPAL_TYPES).await?.is_some() {
        for row in read_all(txn, PRINCIPAL_TYPES, Vec::new()).await? {
            let ty = text_at(&row, 0)?;
            if within(ty, name) {
                principal_types.push(ty.to_owned());
            }
        }
    }
    let empty = tables.is_empty()
        && foreign_keys.is_empty()
        && memberships.is_empty()
        && entities.is_empty()
        && principal_types.is_empty();
    if !empty && !cascade {
        return Err(invalid(format!(
            "namespace {name} isn't empty; drop it with cascade to drop what's in it"
        )));
    }
    for fk in &foreign_keys {
        super::ddl::drop_foreign_key(txn, fk).await?;
        // Later steps read what earlier ones changed.
        txn.next_statement();
    }
    for (member, group) in &memberships {
        super::ddl::drop_membership(txn, member, group).await?;
        // Later steps read what earlier ones changed.
        txn.next_statement();
    }
    for entity in &entities {
        super::ddl::drop_entity(txn, entity).await?;
        // Later steps read what earlier ones changed.
        txn.next_statement();
    }
    for ty in &principal_types {
        super::ddl::drop_principal_type(txn, ty).await?;
        // Later steps read what earlier ones changed.
        txn.next_statement();
    }
    for table in &tables {
        super::ddl::drop_table(txn, table.name()).await?;
        // Later steps read what earlier ones changed.
        txn.next_statement();
    }
    crate::policy::drop_policy_scope(txn, name).await?;
    let table = table(txn).await?;
    rows::delete(txn, &table, &[Datum::Text(name.to_owned())]).await?;
    bump_schema_version(txn).await?;
    Ok(())
}

/// Return the stored name for a new object named `name`: `ns.name` puts it
/// in `ns`, which must exist; an unqualified name goes in the first
/// existing namespace of `path`.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if the namespace doesn't
/// exist, the name has more than two parts, or no namespace of `path` exists.
pub async fn qualify_new(txn: &Txn, path: &[String], name: &str) -> Result<String, EngineError> {
    match name.split_once('.') {
        Some((namespace, rest)) => {
            if rest.contains('.') {
                return Err(invalid(format!("{name} has more than two parts")));
            }
            if !namespace_exists(txn, namespace).await? {
                return Err(invalid(format!("namespace {namespace} does not exist")));
            }
            Ok(stored_name(namespace, rest))
        }
        None => {
            for namespace in path {
                if namespace_exists(txn, namespace).await? {
                    return Ok(stored_name(namespace, name));
                }
            }
            Err(invalid("no namespace of the search path exists".into()))
        }
    }
}

/// What kind of object a name refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    /// A table.
    Table,
    /// An entity type.
    Entity,
    /// A foreign key.
    ForeignKey,
    /// An index.
    Index,
}

/// Return the stored name of an existing object named `name`: `ns.name` is
/// in `ns`; an unqualified name is looked up in each namespace of `path`
/// in turn. If none has it, return the name in the first namespace, so the
/// operation reports it missing.
///
/// # Errors
///
/// Returns a storage error.
pub async fn resolve_existing(
    txn: &Txn,
    path: &[String],
    kind: ObjectKind,
    name: &str,
) -> Result<String, EngineError> {
    if let Some((namespace, rest)) = name.split_once('.') {
        return Ok(stored_name(namespace, rest));
    }
    let mut first = None;
    for namespace in path {
        if !namespace_exists(txn, namespace).await? {
            continue;
        }
        let candidate = stored_name(namespace, name);
        if exists(txn, kind, &candidate).await? {
            return Ok(candidate);
        }
        first.get_or_insert(candidate);
    }
    Ok(first.unwrap_or_else(|| name.to_owned()))
}

/// Return `true` if an object of `kind` is stored as `stored`.
async fn exists(txn: &Txn, kind: ObjectKind, stored: &str) -> Result<bool, EngineError> {
    let key = vec![Datum::Text(stored.to_owned())];
    Ok(match kind {
        ObjectKind::Table => catalog::table(txn, stored).await?.is_some(),
        ObjectKind::Entity => !read_all(txn, ENTITIES, key).await?.is_empty(),
        ObjectKind::ForeignKey => !read_all(txn, FOREIGN_KEYS, key).await?.is_empty(),
        ObjectKind::Index => catalog::tables(txn)
            .await?
            .iter()
            .any(|t| t.indexes().iter().any(|i| i.name() == stored)),
    })
}

/// Return the namespace of stored name `stored`.
#[must_use]
pub fn namespace_of(stored: &str) -> &str {
    split_name(stored).0
}
