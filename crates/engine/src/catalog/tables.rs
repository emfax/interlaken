//! The engine's catalog tables (ADR-0031).
//!
//! Storage's `sys_*` tables hold what storage needs: table, column and index
//! definitions. The IR needs more, which the engine keeps in its own
//! tables, written in the same transactions as storage's:
//!
//! | Table | Primary key | Other columns |
//! | --- | --- | --- |
//! | `engine_catalog` | id (always 1) | schema_version |
//! | `engine_columns` | table_id, column_id | type (IR type text) |
//! | `engine_entities` | name | table_id, key_column |
//! | `engine_attributes` | entity, position | name, column_id, refs (nullable) |
//! | `engine_memberships` | member, grp | edges, edge_member, edge_group, closure, closure_descendant, closure_ancestor, closure_paths |
//! | `engine_functions` | function_id | leakproof |
//! | `engine_privileges` | principal_type, principal_key, privilege | |
//!
//! Names starting with [`ENGINE_PREFIX`] are reserved: clients can't create
//! such tables, and the IR catalog doesn't show them, so clients can
//! neither read nor write the engine's tables (the policy tables among
//! them) except through the engine's API.

use interlaken_storage::Txn;
use interlaken_storage::catalog::{self, ColumnDef, TableDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::rows;

use crate::error::EngineError;

/// The prefix of every engine-owned table name.
pub const ENGINE_PREFIX: &str = "engine_";

/// The one-row table holding the catalog-wide schema version.
pub(crate) const CATALOG: &str = "engine_catalog";
/// IR types of columns.
pub(crate) const COLUMNS: &str = "engine_columns";
/// Entity type declarations.
pub(crate) const ENTITIES: &str = "engine_entities";
/// Entity attributes.
pub(crate) const ATTRIBUTES: &str = "engine_attributes";
/// Membership declarations.
pub(crate) const MEMBERSHIPS: &str = "engine_memberships";
/// Function property overrides set by administrators.
pub(crate) const FUNCTIONS: &str = "engine_functions";
/// Privileges granted to principals.
pub(crate) const PRIVILEGES: &str = "engine_privileges";

/// The primary key of the `engine_catalog` row.
pub(crate) const CATALOG_ROW: i64 = 1;

/// Return whether `name` is reserved for the engine.
#[must_use]
pub fn is_engine_table(name: &str) -> bool {
    name.starts_with(ENGINE_PREFIX)
}

/// Return the stored definition of engine table `name`.
pub(crate) async fn stored(txn: &Txn, name: &str) -> Result<TableDef, EngineError> {
    catalog::table(txn, name)
        .await?
        .ok_or_else(|| EngineError::Catalog(format!("engine table {name} does not exist")))
}

/// Create the engine's catalog tables, with the schema version at 1.
pub(crate) async fn create_catalog_tables(txn: &Txn) -> Result<(), EngineError> {
    let int4 = |n: &str| ColumnDef::new(n, ColumnType::Int4).not_null();
    let int8 = |n: &str| ColumnDef::new(n, ColumnType::Int8).not_null();
    let text = |n: &str| ColumnDef::new(n, ColumnType::Text).not_null();
    let specs = [
        (
            CATALOG,
            vec![int8("id"), int8("schema_version")],
            vec!["id"],
        ),
        (
            COLUMNS,
            vec![int4("table_id"), int4("column_id"), text("type")],
            vec!["table_id", "column_id"],
        ),
        (
            ENTITIES,
            vec![text("name"), int4("table_id"), int4("key_column")],
            vec!["name"],
        ),
        (
            ATTRIBUTES,
            vec![
                text("entity"),
                int4("position"),
                text("name"),
                int4("column_id"),
                ColumnDef::new("refs", ColumnType::Text),
            ],
            vec!["entity", "position"],
        ),
        (
            MEMBERSHIPS,
            vec![
                text("member"),
                text("grp"),
                int4("edges"),
                int4("edge_member"),
                int4("edge_group"),
                int4("closure"),
                int4("closure_descendant"),
                int4("closure_ancestor"),
                int4("closure_paths"),
            ],
            vec!["member", "grp"],
        ),
        (
            FUNCTIONS,
            vec![
                int4("function_id"),
                ColumnDef::new("leakproof", ColumnType::Bool).not_null(),
            ],
            vec!["function_id"],
        ),
        (
            PRIVILEGES,
            vec![
                text("principal_type"),
                text("principal_key"),
                text("privilege"),
            ],
            vec!["principal_type", "principal_key", "privilege"],
        ),
    ];
    for (name, columns, primary_key) in specs {
        catalog::create_table(
            txn,
            TableSpec {
                name: name.into(),
                columns,
                primary_key: primary_key.into_iter().map(Into::into).collect(),
            },
        )
        .await?;
    }
    let table = stored(txn, CATALOG).await?;
    rows::insert(txn, &table, vec![Datum::Int8(CATALOG_ROW), Datum::Int8(1)]).await?;
    Ok(())
}

/// Read the catalog-wide schema version, registering a dependency on it so
/// a writing transaction conflicts with concurrent DDL at every isolation
/// level (ADR-0019).
pub(crate) async fn schema_version(txn: &Txn) -> Result<u64, EngineError> {
    let table = stored(txn, CATALOG).await?;
    let pk = [Datum::Int8(CATALOG_ROW)];
    let row = rows::get(txn, &table, &pk)
        .await?
        .ok_or_else(|| EngineError::Catalog("the schema version row is missing".into()))?;
    rows::register_row_dependency(txn, &table, &pk)?;
    match row.get(1) {
        Some(Datum::Int8(v)) => u64::try_from(*v)
            .map_err(|_| EngineError::Catalog(format!("schema version {v} is out of range"))),
        other => Err(EngineError::Catalog(format!(
            "expected the schema version, found {other:?}"
        ))),
    }
}

/// Increment the catalog-wide schema version, as every DDL statement does.
///
/// Concurrent DDL writes the same row, so one side fails to commit.
pub(crate) async fn bump_schema_version(txn: &Txn) -> Result<u64, EngineError> {
    let next = schema_version(txn)
        .await?
        .checked_add(1)
        .ok_or_else(|| EngineError::Catalog("schema version space exhausted".into()))?;
    let stored_next = i64::try_from(next)
        .map_err(|_| EngineError::Catalog("schema version space exhausted".into()))?;
    let table = stored(txn, CATALOG).await?;
    rows::update(
        txn,
        &table,
        &[Datum::Int8(CATALOG_ROW)],
        vec![Datum::Int8(CATALOG_ROW), Datum::Int8(stored_next)],
    )
    .await?;
    Ok(next)
}

/// Read every row of engine table `name` whose primary key starts with `prefix`.
pub(crate) async fn read_all(
    txn: &Txn,
    name: &str,
    prefix: Vec<Datum>,
) -> Result<Vec<Vec<Datum>>, EngineError> {
    let table = stored(txn, name).await?;
    let mut scan = rows::RowScan::prefix(prefix);
    let mut out = Vec::new();
    loop {
        let batch = rows::scan(txn, &table, &scan).await?;
        out.extend(batch.rows);
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => return Ok(out),
        }
    }
}

/// Read a `text` column.
pub(crate) fn text_at(row: &[Datum], i: usize) -> Result<&str, EngineError> {
    match row.get(i) {
        Some(Datum::Text(s)) => Ok(s),
        other => Err(EngineError::Catalog(format!(
            "expected text, found {other:?}"
        ))),
    }
}

/// Read an `int4` column.
pub(crate) fn int4_at(row: &[Datum], i: usize) -> Result<i32, EngineError> {
    match row.get(i) {
        Some(Datum::Int4(v)) => Ok(*v),
        other => Err(EngineError::Catalog(format!(
            "expected int4, found {other:?}"
        ))),
    }
}

/// Read an `int4` column holding a table or column ID.
pub(crate) fn id_at(row: &[Datum], i: usize) -> Result<u32, EngineError> {
    let v = int4_at(row, i)?;
    u32::try_from(v).map_err(|_| EngineError::Catalog(format!("negative ID {v}")))
}

/// Return an `int4` datum for a table or column ID.
pub(crate) fn id_datum(id: u32) -> Result<Datum, EngineError> {
    i32::try_from(id)
        .map(Datum::Int4)
        .map_err(|_| EngineError::Catalog(format!("ID {id} is out of range")))
}
