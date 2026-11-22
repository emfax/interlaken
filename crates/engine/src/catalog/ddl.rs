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

use interlaken_ir::catalog::{Catalog, ForeignKeyDef, InMemoryCatalog, install_builtins};
use interlaken_ir::text::print_type;
use interlaken_ir::{DataType, QualifiedName, SchemaVersion};
use interlaken_storage::catalog::{self, ColumnDef, TableDef};
use interlaken_storage::encoding::{Datum, Direction, NullOrder, SortOrder};
use interlaken_storage::rows::RowScan;
use interlaken_storage::{ConstraintViolation, Error, Txn, rows};

use super::specs::{ColumnSpec, EntitySpec, ForeignKeySpec, IndexSpec, MembershipSpec, TableSpec};
use super::tables::{
    ATTRIBUTES, COLUMNS, ENTITIES, FOREIGN_KEY_COLUMNS, FOREIGN_KEYS, FUNCTIONS, MEMBERSHIPS,
    bump_schema_version, id_datum, is_engine_table, read_all, stored, text_at,
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
/// type or a membership, or another table's foreign key refers to it, or
/// storage's error if it doesn't exist. The table's own foreign keys are
/// dropped with it.
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
    let mut own = Vec::new();
    for row in read_all(txn, FOREIGN_KEYS, Vec::new()).await? {
        let (from, to) = (
            super::tables::int4_at(&row, 1)?,
            super::tables::int4_at(&row, 2)?,
        );
        if from == id {
            own.push(text_at(&row, 0)?.to_owned());
        } else if to == id {
            return Err(invalid(format!(
                "table {name} is referred to by foreign key {}",
                text_at(&row, 0)?
            )));
        }
    }
    for fk in own {
        delete_foreign_key(txn, &fk).await?;
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
            nulls_distinct: spec.nones_distinct,
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
/// Returns storage's error if there is no such index, or
/// [`EngineError::InvalidDefinition`] if a foreign key refers to its
/// columns. Reloading the catalog afterwards rejects dropping the last
/// index that leads with a foreign key's referencing columns.
pub async fn drop_index(txn: &Txn, name: &str) -> Result<(), EngineError> {
    let def = catalog::drop_index(txn, name).await?;
    check_user_table(def.name())?;
    for row in read_all(txn, FOREIGN_KEYS, Vec::new()).await? {
        if matches!(row.get(3), Some(Datum::Text(index)) if index == name) {
            return Err(invalid(format!(
                "index {name} holds the key foreign key {} refers to",
                text_at(&row, 0)?
            )));
        }
    }
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

/// Return the positions of `columns` in `table`.
fn positions(table: &TableDef, columns: &[String]) -> Result<Vec<usize>, EngineError> {
    columns
        .iter()
        .map(|c| {
            table.column_position(c).ok_or_else(|| {
                EngineError::Storage(
                    interlaken_storage::CatalogError::NoSuchColumn {
                        table: table.name().to_owned(),
                        column: c.clone(),
                    }
                    .into(),
                )
            })
        })
        .collect()
}

/// Return the unique index of `table` whose columns are `columns` in any
/// order, `None` if they are its primary key, or an error if neither.
fn referenced_key(table: &TableDef, columns: &[usize]) -> Result<Option<String>, EngineError> {
    let mut wanted = columns.to_vec();
    wanted.sort_unstable();
    let sorted = |mut v: Vec<usize>| {
        v.sort_unstable();
        v
    };
    if sorted(table.primary_key().to_vec()) == wanted {
        return Ok(None);
    }
    table
        .indexes()
        .iter()
        .find(|i| i.is_unique() && sorted(i.columns().iter().map(|c| c.column).collect()) == wanted)
        .map(|i| Some(i.name().to_owned()))
        .ok_or_else(|| {
            invalid(format!(
                "the referenced columns must be the primary key of {} or the columns of one of \
                 its unique indexes",
                table.name()
            ))
        })
}

/// Create a foreign key (ADR-0035), after checking every existing row satisfies it.
///
/// The declaration is recorded, then the catalog is reloaded, which checks
/// it fits its tables, and then the referencing table's rows are checked.
///
/// # Errors
///
/// - [`EngineError::InvalidDefinition`] if the name is taken, a table is
///   reserved, the column lists differ in length, the referenced columns
///   aren't the referenced table's primary key or a unique index's columns,
///   or the reloaded catalog rejects the declaration: column types that
///   differ, no index on the referencing table that leads with the
///   referencing columns, `SetNull` on columns of plain type, or an entity
///   attribute in the referencing column that refers to another entity type.
/// - [`EngineError::ForeignKey`] if an existing row refers to a row that
///   doesn't exist, or under `MATCH FULL` has some but not all referencing
///   columns NULL.
/// - Storage's error if a table or column doesn't exist.
pub async fn create_foreign_key(txn: &Txn, spec: &ForeignKeySpec) -> Result<(), EngineError> {
    let table = user_table(txn, &spec.table).await?;
    let referenced = user_table(txn, &spec.references).await?;
    if spec.columns.is_empty() || spec.columns.len() != spec.referenced_columns.len() {
        return Err(invalid(format!(
            "foreign key {} needs as many referenced columns as referencing columns, and at \
             least one",
            spec.name
        )));
    }
    let columns = positions(&table, &spec.columns)?;
    let referenced_columns = positions(&referenced, &spec.referenced_columns)?;
    let index = referenced_key(&referenced, &referenced_columns)?;
    rows::insert(
        txn,
        &stored(txn, FOREIGN_KEYS).await?,
        vec![
            Datum::Text(spec.name.clone()),
            table_id(&table),
            table_id(&referenced),
            index.map_or(Datum::Null, Datum::Text),
            Datum::Text(spec.on_delete.name().to_owned()),
            Datum::Bool(spec.match_full),
        ],
    )
    .await
    .map_err(|e| exists_as(e, format!("foreign key {} already exists", spec.name)))?;
    let pairs = stored(txn, FOREIGN_KEY_COLUMNS).await?;
    for (position, (from, to)) in columns.iter().zip(&referenced_columns).enumerate() {
        let id =
            |n: usize| id_datum(u32::try_from(n).map_err(|_| invalid("too many columns".into()))?);
        rows::insert(
            txn,
            &pairs,
            vec![
                Datum::Text(spec.name.clone()),
                id(position)?,
                id(*from)?,
                id(*to)?,
            ],
        )
        .await?;
    }
    bump_schema_version(txn).await?;
    // Reloading checks the declaration fits its tables; it must see this
    // statement's writes.
    txn.next_statement();
    let ir = super::load_catalog(txn).await.map_err(|e| match e {
        EngineError::Catalog(reason) => EngineError::InvalidDefinition(reason),
        other => other,
    })?;
    let fk = ir
        .foreign_keys()
        .into_iter()
        .find(|fk| fk.name.as_str() == spec.name)
        .ok_or_else(|| EngineError::Catalog(format!("foreign key {} vanished", spec.name)))?;
    check_existing_rows(txn, &table, &referenced, &fk).await
}

/// Check every row of `table` refers to an existing row of `referenced` through `fk`.
async fn check_existing_rows(
    txn: &Txn,
    table: &TableDef,
    referenced: &TableDef,
    fk: &ForeignKeyDef,
) -> Result<(), EngineError> {
    let at = |id: interlaken_ir::ColumnId| usize::try_from(id.get()).unwrap_or(usize::MAX);
    let columns: Vec<usize> = fk.columns.iter().map(|&c| at(c)).collect();
    let targets: Vec<usize> = fk.referenced_columns.iter().map(|&c| at(c)).collect();
    let index = match &fk.referenced_index {
        None => None,
        Some(name) => Some(
            referenced
                .indexes()
                .iter()
                .find(|i| i.name() == name.as_str())
                .ok_or_else(|| EngineError::Catalog(format!("index {name} vanished")))?,
        ),
    };
    let key_order: Vec<usize> = match index {
        None => referenced.primary_key().to_vec(),
        Some(index) => index.columns().iter().map(|c| c.column).collect(),
    };
    // For each key column of the referenced key, the referencing column holding its value.
    let sources: Vec<usize> = key_order
        .iter()
        .map(|k| {
            let i = targets
                .iter()
                .position(|t| t == k)
                .expect("checked on reload");
            columns[i]
        })
        .collect();
    let mut scan = RowScan::all();
    loop {
        let batch = rows::scan(txn, table, &scan).await?;
        let mut checked = Vec::new();
        for row in &batch.rows {
            let nulls = columns.iter().filter(|&&c| row[c] == Datum::Null).count();
            if nulls == columns.len() || (nulls > 0 && !fk.match_full) {
                continue;
            }
            if nulls > 0 {
                return Err(EngineError::ForeignKey(format!(
                    "a row of {} has some but not all of the columns of foreign key {}, which \
                     matches FULL, NULL",
                    table.name(),
                    fk.name
                )));
            }
            checked.push(sources.iter().map(|&c| row[c].clone()).collect::<Vec<_>>());
        }
        let found: Vec<bool> = match index {
            None => rows::get_many(txn, referenced, &checked)
                .await?
                .iter()
                .map(Option::is_some)
                .collect(),
            Some(index) => {
                let scans: Vec<RowScan> = checked
                    .iter()
                    .map(|key| RowScan {
                        limit_rows: 1,
                        ..RowScan::prefix(key.clone())
                    })
                    .collect();
                rows::scan_index_many(txn, referenced, index, &scans)
                    .await?
                    .iter()
                    .map(|b| !b.primary_keys.is_empty())
                    .collect()
            }
        };
        if let Some(missing) = found.iter().position(|f| !f) {
            let shown: Vec<String> = key_order
                .iter()
                .zip(&checked[missing])
                .map(|(&k, v)| format!("{} = {v:?}", referenced.columns()[k].name))
                .collect();
            return Err(EngineError::ForeignKey(format!(
                "foreign key {}: a row of {} refers to a row of {} with {}, which doesn't exist",
                fk.name,
                table.name(),
                referenced.name(),
                shown.join(", ")
            )));
        }
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => return Ok(()),
        }
    }
}

/// Delete foreign key `name`'s declaration; return whether it existed.
async fn delete_foreign_key(txn: &Txn, name: &str) -> Result<bool, EngineError> {
    let key = vec![Datum::Text(name.to_owned())];
    if !rows::delete(txn, &stored(txn, FOREIGN_KEYS).await?, &key).await? {
        return Ok(false);
    }
    let pairs = stored(txn, FOREIGN_KEY_COLUMNS).await?;
    for row in read_all(txn, FOREIGN_KEY_COLUMNS, key).await? {
        rows::delete(txn, &pairs, &row[..2]).await?;
    }
    Ok(true)
}

/// Drop a foreign key; its tables and indexes stay.
///
/// # Errors
///
/// Returns [`EngineError::InvalidDefinition`] if there is no such foreign key.
pub async fn drop_foreign_key(txn: &Txn, name: &str) -> Result<(), EngineError> {
    if !delete_foreign_key(txn, name).await? {
        return Err(invalid(format!("foreign key {name} does not exist")));
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
