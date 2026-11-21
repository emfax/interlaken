//! Engine DDL: storage's catalog and the engine's, changed together (ADR-0031).
//!
//! Each operation changes storage's `sys_*` tables (through storage's DDL)
//! and the engine's catalog tables in the caller's transaction, and bumps
//! the catalog-wide schema version, so resolved plans pinned to the old
//! version stop verifying and concurrent DDL conflicts. The operations
//! check names and references; the caller reloads the catalog afterwards
//! ([`load_catalog`](super::load_catalog)), which checks the declarations
//! fit together, and fails the statement if they don't.
//!
//! Privileges are the caller's concern: the session lets only
//! administrators run DDL.

use interlaken_ir::catalog::{Catalog, InMemoryCatalog, install_builtins};
use interlaken_ir::text::print_type;
use interlaken_ir::{DataType, QualifiedName, SchemaVersion};
use interlaken_storage::catalog::{self, ColumnDef, TableDef};
use interlaken_storage::encoding::{Datum, Direction, NullOrder, SortOrder};
use interlaken_storage::{ConstraintViolation, Error, Txn, rows};

use super::specs::{ColumnSpec, EntitySpec, IndexSpec, MembershipSpec, TableSpec};
use super::tables::{
    ATTRIBUTES, COLUMNS, ENTITIES, FUNCTIONS, MEMBERSHIPS, bump_schema_version, id_datum,
    is_engine_table, read_all, stored, text_at,
};
use super::types::stored_type;
use crate::error::EngineError;

/// Construct an invalid-definition error.
fn invalid(message: String) -> EngineError {
    EngineError::InvalidDefinition(message)
}

/// Reject a table name reserved for the engine.
fn check_user_table(name: &str) -> Result<(), EngineError> {
    if is_engine_table(name) {
        return Err(invalid(format!(
            "table names starting with \"{}\" are reserved",
            super::tables::ENGINE_PREFIX
        )));
    }
    Ok(())
}

/// Return the stored definition of user table `name`.
async fn user_table(txn: &Txn, name: &str) -> Result<TableDef, EngineError> {
    check_user_table(name)?;
    catalog::table(txn, name)
        .await?
        .ok_or_else(|| EngineError::Storage(catalog_error(name)))
}

/// Return storage's error for a missing table.
fn catalog_error(name: &str) -> Error {
    interlaken_storage::CatalogError::NoSuchTable(name.to_owned()).into()
}

/// Return the position of column `column` of `table`, as an `int4` datum.
fn column_of(table: &TableDef, column: &str) -> Result<Datum, EngineError> {
    let position = table.column_position(column).ok_or_else(|| {
        EngineError::Storage(
            interlaken_storage::CatalogError::NoSuchColumn {
                table: table.name().to_owned(),
                column: column.to_owned(),
            }
            .into(),
        )
    })?;
    id_datum(u32::try_from(position).map_err(|_| invalid("too many columns".into()))?)
}

/// Return the ID of `table` as an `int4` datum.
fn table_id(table: &TableDef) -> Datum {
    Datum::Int4(table.id().get())
}

/// Replace a duplicate-key error on an engine table with `exists`.
fn exists_as(error: Error, exists: String) -> EngineError {
    match error {
        Error::Constraint(
            ConstraintViolation::PrimaryKey { .. } | ConstraintViolation::Unique { .. },
        ) => invalid(exists),
        other => other.into(),
    }
}

/// Record the IR type of column `position` of table `table`.
async fn insert_column_type(
    txn: &Txn,
    table: i32,
    position: usize,
    column: &ColumnSpec,
) -> Result<(), EngineError> {
    let position = i32::try_from(position).map_err(|_| invalid("too many columns".into()))?;
    rows::insert(
        txn,
        &stored(txn, COLUMNS).await?,
        vec![
            Datum::Int4(table),
            Datum::Int4(position),
            Datum::Text(print_type(&column.ty)),
        ],
    )
    .await?;
    Ok(())
}

/// Create a table.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if the name is reserved or a
/// column's type can't be stored, or storage's error for an invalid or
/// duplicate definition.
pub async fn create_table(txn: &Txn, spec: &TableSpec) -> Result<(), EngineError> {
    check_user_table(&spec.name)?;
    let columns = spec
        .columns
        .iter()
        .map(|c| {
            let column = ColumnDef::new(c.name.clone(), stored_type(&c.name, &c.ty)?);
            Ok(if c.ty.optional {
                column
            } else {
                column.not_null()
            })
        })
        .collect::<Result<_, EngineError>>()?;
    let def = catalog::create_table(
        txn,
        catalog::TableSpec {
            name: spec.name.clone(),
            columns,
            primary_key: spec.primary_key.clone(),
        },
    )
    .await?;
    for (position, column) in spec.columns.iter().enumerate() {
        insert_column_type(txn, def.id().get(), position, column).await?;
    }
    bump_schema_version(txn).await?;
    Ok(())
}

/// Drop a table with its rows and indexes.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if the table backs an entity
/// type or a membership, or storage's error if it doesn't exist.
pub async fn drop_table(txn: &Txn, name: &str) -> Result<(), EngineError> {
    let def = user_table(txn, name).await?;
    let id = def.id().get();
    for row in read_all(txn, ENTITIES, Vec::new()).await? {
        if super::tables::int4_at(&row, 1)? == id {
            return Err(invalid(format!(
                "table {name} backs entity type {}",
                text_at(&row, 0)?
            )));
        }
    }
    for row in read_all(txn, MEMBERSHIPS, Vec::new()).await? {
        if super::tables::int4_at(&row, 2)? == id || super::tables::int4_at(&row, 5)? == id {
            return Err(invalid(format!(
                "table {name} holds the membership of {} in {}",
                text_at(&row, 0)?,
                text_at(&row, 1)?
            )));
        }
    }
    catalog::drop_table(txn, name).await?;
    let columns = stored(txn, COLUMNS).await?;
    for row in read_all(txn, COLUMNS, vec![Datum::Int4(id)]).await? {
        rows::delete(txn, &columns, &row[..2]).await?;
    }
    bump_schema_version(txn).await?;
    Ok(())
}

/// Add a nullable column at the end of a table; existing rows read it as NULL.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if the type can't be stored,
/// or storage's error (the column is NOT NULL or exists, or the table
/// doesn't).
pub async fn add_column(txn: &Txn, table: &str, column: &ColumnSpec) -> Result<(), EngineError> {
    let def = user_table(txn, table).await?;
    let mut stored_column =
        ColumnDef::new(column.name.clone(), stored_type(&column.name, &column.ty)?);
    stored_column.nullable = column.ty.optional;
    let def = catalog::add_column(txn, def.name(), stored_column).await?;
    insert_column_type(txn, def.id().get(), def.columns().len() - 1, column).await?;
    bump_schema_version(txn).await?;
    Ok(())
}

/// Create an index and fill it from the table's rows.
///
/// # Errors
///
/// Returns storage's error for an invalid or duplicate index, or a unique
/// index over duplicate rows.
pub async fn create_index(txn: &Txn, spec: &IndexSpec) -> Result<(), EngineError> {
    user_table(txn, &spec.table).await?;
    let columns = spec
        .columns
        .iter()
        .map(|k| {
            let order = SortOrder::new(
                if k.descending {
                    Direction::Desc
                } else {
                    Direction::Asc
                },
                if k.nulls_first {
                    NullOrder::First
                } else {
                    NullOrder::Last
                },
            );
            (k.column.clone(), order)
        })
        .collect();
    catalog::create_index(
        txn,
        catalog::IndexSpec {
            name: spec.name.clone(),
            table: spec.table.clone(),
            columns,
            unique: spec.unique,
        },
    )
    .await?;
    bump_schema_version(txn).await?;
    Ok(())
}

/// Drop an index.
///
/// # Errors
///
/// Returns storage's error if there is no such index.
pub async fn drop_index(txn: &Txn, name: &str) -> Result<(), EngineError> {
    let def = catalog::drop_index(txn, name).await?;
    check_user_table(def.name())?;
    bump_schema_version(txn).await?;
    Ok(())
}

/// Declare an entity type backed by a table (ADR-0017).
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if the name is taken, or
/// storage's error if the table or a column doesn't exist. Reloading the
/// catalog afterwards rejects a table that already backs an entity type.
pub async fn declare_entity(txn: &Txn, spec: &EntitySpec) -> Result<(), EngineError> {
    let table = user_table(txn, &spec.table).await?;
    let key = column_of(&table, &spec.key)?;
    rows::insert(
        txn,
        &stored(txn, ENTITIES).await?,
        vec![Datum::Text(spec.name.clone()), table_id(&table), key],
    )
    .await
    .map_err(|e| exists_as(e, format!("entity type {} already exists", spec.name)))?;
    let attributes = stored(txn, ATTRIBUTES).await?;
    for (position, attribute) in spec.attributes.iter().enumerate() {
        let position =
            i32::try_from(position).map_err(|_| invalid("too many attributes".into()))?;
        if spec.attributes[..usize::try_from(position).unwrap_or(0)]
            .iter()
            .any(|a| a.name == attribute.name)
        {
            return Err(invalid(format!(
                "entity type {} repeats attribute {}",
                spec.name, attribute.name
            )));
        }
        rows::insert(
            txn,
            &attributes,
            vec![
                Datum::Text(spec.name.clone()),
                Datum::Int4(position),
                Datum::Text(attribute.name.clone()),
                column_of(&table, &attribute.column)?,
                attribute
                    .references
                    .clone()
                    .map_or(Datum::Null, Datum::Text),
            ],
        )
        .await?;
    }
    bump_schema_version(txn).await?;
    Ok(())
}

/// Remove an entity type's declaration; its table stays.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if there is no such entity
/// type or a membership uses it.
pub async fn drop_entity(txn: &Txn, name: &str) -> Result<(), EngineError> {
    for row in read_all(txn, MEMBERSHIPS, Vec::new()).await? {
        if text_at(&row, 0)? == name || text_at(&row, 1)? == name {
            return Err(invalid(format!(
                "entity type {name} is used by the membership of {} in {}",
                text_at(&row, 0)?,
                text_at(&row, 1)?
            )));
        }
    }
    let key = vec![Datum::Text(name.to_owned())];
    if !rows::delete(txn, &stored(txn, ENTITIES).await?, &key).await? {
        return Err(invalid(format!("entity type {name} does not exist")));
    }
    let attributes = stored(txn, ATTRIBUTES).await?;
    for row in read_all(txn, ATTRIBUTES, key).await? {
        rows::delete(txn, &attributes, &row[..2]).await?;
    }
    bump_schema_version(txn).await?;
    Ok(())
}

/// Declare a membership between two entity types (ADR-0017).
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if it is already declared,
/// or storage's error if a table or column doesn't exist. Reloading the
/// catalog afterwards rejects, with [`EngineError::InvalidDefinition`],
/// undeclared entity types and columns whose types don't match: member
/// and descendant columns must have the member entity's key type, group
/// and ancestor columns the group entity's key type, and the path count
/// `int8`.
pub async fn declare_membership(txn: &Txn, spec: &MembershipSpec) -> Result<(), EngineError> {
    let edges = user_table(txn, &spec.edges).await?;
    let closure = user_table(txn, &spec.closure).await?;
    rows::insert(
        txn,
        &stored(txn, MEMBERSHIPS).await?,
        vec![
            Datum::Text(spec.member.clone()),
            Datum::Text(spec.group.clone()),
            table_id(&edges),
            column_of(&edges, &spec.edge_columns.0)?,
            column_of(&edges, &spec.edge_columns.1)?,
            table_id(&closure),
            column_of(&closure, &spec.closure_columns.0)?,
            column_of(&closure, &spec.closure_columns.1)?,
            column_of(&closure, &spec.closure_columns.2)?,
        ],
    )
    .await
    .map_err(|e| {
        exists_as(
            e,
            format!(
                "the membership of {} in {} already exists",
                spec.member, spec.group
            ),
        )
    })?;
    bump_schema_version(txn).await?;
    Ok(())
}

/// Remove a membership's declaration; its tables stay.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if there is no such membership.
pub async fn drop_membership(txn: &Txn, member: &str, group: &str) -> Result<(), EngineError> {
    let key = [
        Datum::Text(member.to_owned()),
        Datum::Text(group.to_owned()),
    ];
    if !rows::delete(txn, &stored(txn, MEMBERSHIPS).await?, &key).await? {
        return Err(invalid(format!(
            "the membership of {member} in {group} does not exist"
        )));
    }
    bump_schema_version(txn).await?;
    Ok(())
}

/// Mark the function overload `name(args)` leakproof or not (ADR-0011).
///
/// Leakproof functions may run below security barriers, so only
/// administrators may call this (the session checks).
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if no overload matches exactly.
pub async fn set_function_leakproof(
    txn: &Txn,
    name: &QualifiedName,
    args: &[DataType],
    leakproof: bool,
) -> Result<(), EngineError> {
    let mut builtins = InMemoryCatalog::new(SchemaVersion::new(0));
    install_builtins(&mut builtins).map_err(|e| EngineError::Catalog(e.to_string()))?;
    let def = builtins.function_by_name(name, args).ok_or_else(|| {
        let args: Vec<String> = args
            .iter()
            .map(|a| print_type(&a.clone().option()))
            .collect();
        invalid(format!(
            "function {name}({}) does not exist",
            args.join(", ")
        ))
    })?;
    let table = stored(txn, FUNCTIONS).await?;
    let id = id_datum(def.id.get())?;
    let row = vec![id.clone(), Datum::Bool(leakproof)];
    if !rows::update(txn, &table, std::slice::from_ref(&id), row.clone()).await? {
        rows::insert(txn, &table, row).await?;
    }
    bump_schema_version(txn).await?;
    Ok(())
}
