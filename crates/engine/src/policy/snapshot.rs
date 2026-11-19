//! Copying entity data from storage for local policy analysis.

use interlaken_ir::catalog::Catalog;
use interlaken_ir::interp::{Database, Datum};
use interlaken_storage::Txn;
use interlaken_storage::catalog;
use interlaken_storage::encoding::Datum as Stored;
use interlaken_storage::rows::{self, RowScan};

use crate::error::EngineError;

/// Convert a stored value.
fn datum(d: &Stored) -> Datum {
    match d {
        Stored::Null => Datum::Null,
        Stored::Bool(v) => Datum::Bool(*v),
        Stored::Int2(v) => Datum::Int2(*v),
        Stored::Int4(v) => Datum::Int4(*v),
        Stored::Int8(v) => Datum::Int8(*v),
        Stored::Float8(v) => Datum::Float8(*v),
        Stored::Text(v) => Datum::Text(v.clone()),
        Stored::Bytea(v) => Datum::Bytea(v.clone()),
        Stored::Array(items) => Datum::Array(items.iter().map(datum).collect()),
    }
}

/// Read every entity table and closure table through `txn` into an interpreter database.
///
/// Impact analysis runs on this snapshot; reading through the transaction
/// keeps it consistent with the policies being compared.
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails or a declared table is not stored.
pub async fn entity_snapshot(txn: &Txn, catalog: &dyn Catalog) -> Result<Database, EngineError> {
    let mut tables = Vec::new();
    for entity in catalog.entities() {
        tables.push(entity.table);
        for membership in catalog.memberships_of(&entity.name) {
            tables.push(membership.closure);
        }
    }
    let mut db = Database::new();
    for id in tables {
        let def = catalog.table_by_id(id).ok_or_else(|| {
            EngineError::Catalog(format!("IR table {} is not declared", id.get()))
        })?;
        let name = def.name.last().to_string();
        let stored = catalog::table(txn, &name)
            .await?
            .ok_or_else(|| EngineError::Catalog(format!("table {name} is not stored")))?;
        let mut out = Vec::new();
        let mut scan = RowScan::all();
        loop {
            let batch = rows::scan(txn, &stored, &scan).await?;
            out.extend(batch.rows.iter().map(|r| r.iter().map(datum).collect()));
            match batch.resume {
                Some(resume) => scan.resume = Some(resume),
                None => break,
            }
        }
        db.set_rows(id, out);
    }
    Ok(db)
}
