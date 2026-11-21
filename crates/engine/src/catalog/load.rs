//! Reading the IR catalog from storage (ADR-0013 phase 3, ADR-0031).

use std::collections::HashMap;

use interlaken_ir::catalog::{
    AttributeDef, EntityDef, InMemoryCatalog, IndexColumn, IndexDef, MembershipDef, TableDef,
    install_builtins,
};
use interlaken_ir::{ColumnId, FunctionId, Ident, SchemaVersion, TableId, Type};
use interlaken_storage::Txn;
use interlaken_storage::catalog;
use interlaken_storage::encoding::{Datum, Direction, NullOrder};

use super::tables::{
    ATTRIBUTES, COLUMNS, ENTITIES, FUNCTIONS, MEMBERSHIPS, id_at, is_engine_table, read_all,
    schema_version, text_at,
};
use super::types::derived_type;
use crate::error::EngineError;

/// Read the IR catalog as of `txn`'s snapshot (and its own writes).
///
/// The catalog holds the built-in functions (with administrators'
/// property changes), every user table with its recorded IR column types,
/// and the entity and membership declarations, at the catalog-wide schema
/// version. Engine tables are left out. Every read goes through `txn`, and
/// the schema version row becomes a dependency.
///
/// # Errors
///
/// Returns [`EngineError::Catalog`] if the stored catalog is inconsistent,
/// or a storage error.
pub async fn load_catalog(txn: &Txn) -> Result<InMemoryCatalog, EngineError> {
    let version = schema_version(txn).await?;
    let mut ir = InMemoryCatalog::new(SchemaVersion::new(version));
    let invalid = |e: interlaken_ir::catalog::CatalogError| EngineError::Catalog(e.to_string());
    install_builtins(&mut ir).map_err(invalid)?;
    for row in read_all(txn, FUNCTIONS, Vec::new()).await? {
        let id = FunctionId::new(id_at(&row, 0)?);
        let Some(def) = interlaken_ir::catalog::Catalog::function_by_id(&ir, id) else {
            return Err(EngineError::Catalog(format!(
                "function {} has properties but no definition",
                id.get()
            )));
        };
        let mut properties = def.properties.clone();
        properties.leakproof = matches!(row.get(1), Some(Datum::Bool(true)));
        ir.set_function_properties(id, properties)
            .map_err(invalid)?;
    }

    let mut ir_types: HashMap<(u32, u32), Type> = HashMap::new();
    for row in read_all(txn, COLUMNS, Vec::new()).await? {
        let ty = interlaken_ir::text::parse_type(text_at(&row, 2)?)
            .map_err(|e| EngineError::Catalog(format!("stored column type: {e}")))?;
        ir_types.insert((id_at(&row, 0)?, id_at(&row, 1)?), ty);
    }
    for stored in catalog::tables(txn).await? {
        if is_engine_table(stored.name()) {
            continue;
        }
        let id = u32::try_from(stored.id().get())
            .map_err(|_| EngineError::Catalog("negative table ID".into()))?;
        let mut def = TableDef::new(TableId::new(id), stored.name());
        for (position, column) in stored.columns().iter().enumerate() {
            let column_id = u32::try_from(position)
                .map_err(|_| EngineError::Catalog("too many columns".into()))?;
            let ty = ir_types.remove(&(id, column_id)).map_or_else(
                || derived_type(column.ty, column.nullable),
                |ty| Type {
                    optional: column.nullable,
                    ..ty
                },
            );
            def = def.column(column.name.as_str(), ty);
        }
        def.primary_key = stored
            .primary_key()
            .iter()
            .map(|&p| column_id(p))
            .collect::<Result<_, _>>()?;
        for index in stored.indexes() {
            let columns = index
                .columns()
                .iter()
                .map(|c| {
                    Ok(IndexColumn {
                        column: column_id(c.column)?,
                        descending: c.order.direction == Direction::Desc,
                        nulls_first: c.order.nulls == NullOrder::First,
                    })
                })
                .collect::<Result<_, EngineError>>()?;
            def.indexes.push(IndexDef {
                name: index.name().into(),
                columns,
                unique: index.is_unique(),
            });
        }
        ir.add_table(def).map_err(invalid)?;
    }

    let mut attributes: HashMap<String, Vec<AttributeDef>> = HashMap::new();
    for row in read_all(txn, ATTRIBUTES, Vec::new()).await? {
        attributes
            .entry(text_at(&row, 0)?.to_owned())
            .or_default()
            .push(AttributeDef {
                name: text_at(&row, 2)?.into(),
                column: ColumnId::new(id_at(&row, 3)?),
                references: match row.get(4) {
                    Some(Datum::Text(t)) => Some(Ident::new(t.as_str())),
                    _ => None,
                },
            });
    }
    for row in read_all(txn, ENTITIES, Vec::new()).await? {
        let name = text_at(&row, 0)?;
        ir.add_entity(EntityDef {
            name: name.into(),
            table: TableId::new(id_at(&row, 1)?),
            key: ColumnId::new(id_at(&row, 2)?),
            attributes: attributes.remove(name).unwrap_or_default(),
        })
        .map_err(invalid)?;
    }
    if let Some(entity) = attributes.keys().next() {
        return Err(EngineError::Catalog(format!(
            "attributes of undeclared entity type {entity}"
        )));
    }
    for row in read_all(txn, MEMBERSHIPS, Vec::new()).await? {
        let column = |i| id_at(&row, i).map(ColumnId::new);
        ir.add_membership(MembershipDef {
            member: text_at(&row, 0)?.into(),
            group: text_at(&row, 1)?.into(),
            edges: TableId::new(id_at(&row, 2)?),
            edge_columns: (column(3)?, column(4)?),
            closure: TableId::new(id_at(&row, 5)?),
            closure_columns: (column(6)?, column(7)?, column(8)?),
        })
        .map_err(invalid)?;
    }
    Ok(ir)
}

/// Return the IR column ID of the column at `position`.
fn column_id(position: usize) -> Result<ColumnId, EngineError> {
    u32::try_from(position)
        .map(ColumnId::new)
        .map_err(|_| EngineError::Catalog("column position out of range".into()))
}
