//! Loading the principal's entity data (ADR-0017, ADR-0019).
//!
//! - **Principal loading.** At transaction start the engine reads the
//!   principal's entity row and its closure rows (every group it belongs
//!   to, transitively) through the transaction. Reads join the read set,
//!   so under SERIALIZABLE a concurrent change to the principal's
//!   attributes or memberships (a role revocation) makes the transaction
//!   fail at commit with a serialization failure if it wrote anything.
//!   Closure rows are scanned by the principal's key prefix, so unrelated
//!   memberships do not conflict.

use interlaken_ir::Ident;
use interlaken_ir::catalog::Catalog;
use interlaken_ir::security::{EntityKey, EntityStore, EntityUid, SecValue};
use interlaken_storage::Txn;
use interlaken_storage::catalog;
use interlaken_storage::encoding::Datum;
use interlaken_storage::rows::{self, RowScan};

use super::error::EngineError;

/// The principal's entity data, read through a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedEntities {
    /// The principal.
    pub principal: EntityUid,
    /// The principal's attributes, or `None` if it does not exist.
    pub attributes: Option<Vec<(Ident, SecValue)>>,
    /// Every group the principal belongs to, transitively.
    pub ancestors: Vec<EntityUid>,
}

impl EntityStore for LoadedEntities {
    /// Return the principal's attributes; other entities are not loaded.
    fn attributes(&self, uid: &EntityUid) -> Option<Vec<(Ident, SecValue)>> {
        if *uid == self.principal {
            self.attributes.clone()
        } else {
            None
        }
    }

    /// Return the principal's ancestors; other entities' are not loaded.
    fn ancestors(&self, uid: &EntityUid) -> Vec<EntityUid> {
        if *uid == self.principal {
            self.ancestors.clone()
        } else {
            Vec::new()
        }
    }
}

/// Convert an entity key to a stored datum.
fn key_datum(key: &EntityKey) -> Datum {
    match key {
        EntityKey::Long(v) => Datum::Int8(*v),
        EntityKey::Str(v) => Datum::Text(v.clone()),
    }
}

/// Convert a stored datum to an entity key.
fn datum_key(d: &Datum) -> Option<EntityKey> {
    Some(match d {
        Datum::Int2(v) => EntityKey::Long(i64::from(*v)),
        Datum::Int4(v) => EntityKey::Long(i64::from(*v)),
        Datum::Int8(v) => EntityKey::Long(*v),
        Datum::Text(v) => EntityKey::Str(v.clone()),
        _ => return None,
    })
}

/// Look up the stored table backing IR table `id`, by name.
async fn stored_table(
    txn: &Txn,
    catalog: &dyn Catalog,
    id: interlaken_ir::TableId,
) -> Result<catalog::TableDef, EngineError> {
    let def = catalog
        .table_by_id(id)
        .ok_or_else(|| EngineError::Catalog(format!("IR table {} is not declared", id.get())))?;
    let name = def.name.last().to_string();
    catalog::table(txn, &name)
        .await?
        .ok_or_else(|| EngineError::Catalog(format!("table {name} is not stored")))
}

/// Read the principal's entity row and closure rows through `txn` (ADR-0017).
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails or the IR catalog names
/// tables or columns storage does not have.
pub async fn load_principal(
    txn: &Txn,
    catalog: &dyn Catalog,
    principal: &EntityUid,
) -> Result<LoadedEntities, EngineError> {
    let Some(entity) = catalog.entity_by_name(&principal.ty) else {
        return Ok(LoadedEntities {
            principal: principal.clone(),
            attributes: None,
            ancestors: Vec::new(),
        });
    };
    let table = stored_table(txn, catalog, entity.table).await?;
    let row = rows::get(txn, &table, &[key_datum(&principal.key)]).await?;
    let attributes = row.map(|row| {
        entity
            .attributes
            .iter()
            .filter_map(|a| {
                let d = row.get(usize::try_from(a.column.get()).ok()?)?;
                let value = match (&a.references, d) {
                    (_, Datum::Null) => return None,
                    (Some(ty), d) => SecValue::Entity(EntityUid {
                        ty: ty.clone(),
                        key: datum_key(d)?,
                    }),
                    (None, Datum::Bool(b)) => SecValue::Bool(*b),
                    (None, Datum::Text(s)) => SecValue::Str(s.clone()),
                    (None, d) => match datum_key(d)? {
                        EntityKey::Long(v) => SecValue::Long(v),
                        EntityKey::Str(_) => return None,
                    },
                };
                Some((a.name.clone(), value))
            })
            .collect()
    });
    let mut ancestors = Vec::new();
    for membership in catalog.memberships_of(&principal.ty) {
        let closure = stored_table(txn, catalog, membership.closure).await?;
        let (descendant, ancestor, _) = membership.closure_columns;
        let (d, a) = (
            usize::try_from(descendant.get()).unwrap_or(usize::MAX),
            usize::try_from(ancestor.get()).unwrap_or(usize::MAX),
        );
        // Scan only the principal's rows when the key starts with the descendant.
        let prefix = if closure.primary_key().first() == Some(&d) {
            vec![key_datum(&principal.key)]
        } else {
            Vec::new()
        };
        let mut scan = RowScan::prefix(prefix);
        loop {
            let batch = rows::scan(txn, &closure, &scan).await?;
            for row in &batch.rows {
                if datum_key(&row[d]).as_ref() == Some(&principal.key)
                    && let Some(key) = datum_key(&row[a])
                {
                    ancestors.push(EntityUid {
                        ty: membership.group.clone(),
                        key,
                    });
                }
            }
            match batch.resume {
                Some(resume) => scan.resume = Some(resume),
                None => break,
            }
        }
    }
    Ok(LoadedEntities {
        principal: principal.clone(),
        attributes,
        ancestors,
    })
}
