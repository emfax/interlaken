//! The user schema as DDL statements, which recreate it (ADR-0043).

use std::collections::BTreeSet;

use interlaken_ir::catalog::{
    Catalog, ColumnDef as IrColumn, EntityDef, InMemoryCatalog, TableDef as IrTable,
    install_builtins,
};
use interlaken_ir::ddl::{
    AttributeSpec, ColumnSpec, DdlStatement, EntitySpec, ForeignKeySpec, IndexKey, IndexSpec,
    MembershipSpec, TableSpec,
};
use interlaken_ir::{ColumnId, FunctionId, SchemaVersion, TableId};
use interlaken_storage::Txn;

use super::load::load_catalog;
use super::namespaces::namespace_owners;
use super::tables::{FUNCTIONS, id_at, read_all};
use crate::EngineError;

/// Return the error for a catalog that refers to something missing.
fn dangling(what: String) -> EngineError {
    EngineError::Catalog(format!("the catalog refers to a missing {what}"))
}

/// Return `table`'s column `id`.
fn column(table: &IrTable, id: ColumnId) -> Result<&IrColumn, EngineError> {
    table
        .columns
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| dangling(format!("column of {}", table.name.joined())))
}

/// Return the name of `table`'s column `id`.
fn column_name(table: &IrTable, id: ColumnId) -> Result<String, EngineError> {
    Ok(column(table, id)?.name.as_str().to_owned())
}

/// Return table `id` of `catalog`.
fn table(catalog: &InMemoryCatalog, id: TableId) -> Result<std::sync::Arc<IrTable>, EngineError> {
    catalog
        .table_by_id(id)
        .ok_or_else(|| dangling(format!("table {}", id.get())))
}

/// Return the statements creating `table` and its indexes.
fn table_statements(table: &IrTable) -> Result<Vec<DdlStatement>, EngineError> {
    let name = table.name.joined();
    let mut out = vec![DdlStatement::CreateTable(TableSpec {
        name: name.clone(),
        columns: table
            .columns
            .iter()
            .map(|c| ColumnSpec::new(c.name.as_str(), c.ty.clone()))
            .collect(),
        primary_key: table
            .primary_key
            .iter()
            .map(|&id| column_name(table, id))
            .collect::<Result<_, _>>()?,
    })];
    for index in &table.indexes {
        out.push(DdlStatement::CreateIndex(IndexSpec {
            name: index.name.as_str().to_owned(),
            table: name.clone(),
            columns: index
                .columns
                .iter()
                .map(|key| {
                    Ok(IndexKey {
                        column: column_name(table, key.column)?,
                        descending: key.descending,
                        nulls_first: key.nulls_first,
                    })
                })
                .collect::<Result<_, EngineError>>()?,
            unique: index.unique,
            nones_distinct: index.nones_distinct,
        }));
    }
    Ok(out)
}

/// Return `entity`'s declaration.
fn entity_spec(catalog: &InMemoryCatalog, entity: &EntityDef) -> Result<EntitySpec, EngineError> {
    let table = table(catalog, entity.table)?;
    Ok(EntitySpec {
        name: entity.name.as_str().to_owned(),
        table: table.name.joined(),
        key: column_name(&table, entity.key)?,
        attributes: entity
            .attributes
            .iter()
            .map(|a| {
                Ok(AttributeSpec {
                    name: a.name.as_str().to_owned(),
                    column: column_name(&table, a.column)?,
                    references: a.references.as_ref().map(|r| r.as_str().to_owned()),
                })
            })
            .collect::<Result<_, EngineError>>()?,
    })
}

/// Return the entity declarations, each after the entity types its
/// attributes refer to, which declaring it requires.
fn entity_statements(catalog: &InMemoryCatalog) -> Result<Vec<DdlStatement>, EngineError> {
    let mut pending: Vec<EntitySpec> = catalog
        .entities()
        .iter()
        .map(|e| entity_spec(catalog, e))
        .collect::<Result<_, _>>()?;
    pending.sort_by(|a, b| a.name.cmp(&b.name));
    let mut declared = BTreeSet::new();
    let mut out = Vec::new();
    while !pending.is_empty() {
        let ready = pending.iter().position(|spec| {
            spec.attributes.iter().all(|a| {
                a.references
                    .as_ref()
                    .is_none_or(|r| declared.contains(r) || *r == spec.name)
            })
        });
        // A cycle can't be declared through the API; emit the rest as they are.
        let spec = pending.remove(ready.unwrap_or(0));
        declared.insert(spec.name.clone());
        out.push(DdlStatement::DeclareEntity(spec));
    }
    Ok(out)
}

/// Return the membership declarations.
fn membership_statements(catalog: &InMemoryCatalog) -> Result<Vec<DdlStatement>, EngineError> {
    let mut out = Vec::new();
    let mut entities = catalog.entities();
    entities.sort_by(|a, b| a.name.cmp(&b.name));
    for entity in entities {
        for m in catalog.memberships_of(&entity.name) {
            let edges = table(catalog, m.edges)?;
            let closure = table(catalog, m.closure)?;
            out.push(DdlStatement::DeclareMembership(MembershipSpec {
                member: m.member.as_str().to_owned(),
                group: m.group.as_str().to_owned(),
                edges: edges.name.joined(),
                edge_columns: (
                    column_name(&edges, m.edge_columns.0)?,
                    column_name(&edges, m.edge_columns.1)?,
                ),
                closure: closure.name.joined(),
                closure_columns: (
                    column_name(&closure, m.closure_columns.0)?,
                    column_name(&closure, m.closure_columns.1)?,
                    column_name(&closure, m.closure_columns.2)?,
                ),
            }));
        }
    }
    Ok(out)
}

/// Return the foreign key declarations, by name.
fn foreign_key_statements(catalog: &InMemoryCatalog) -> Result<Vec<DdlStatement>, EngineError> {
    let mut keys = catalog.foreign_keys();
    keys.sort_by(|a, b| a.name.cmp(&b.name));
    keys.iter()
        .map(|fk| {
            let from = table(catalog, fk.table)?;
            let to = table(catalog, fk.referenced)?;
            Ok(DdlStatement::CreateForeignKey(ForeignKeySpec {
                name: fk.name.as_str().to_owned(),
                table: from.name.joined(),
                columns: fk
                    .columns
                    .iter()
                    .map(|&c| column_name(&from, c))
                    .collect::<Result<_, _>>()?,
                references: to.name.joined(),
                referenced_columns: fk
                    .referenced_columns
                    .iter()
                    .map(|&c| column_name(&to, c))
                    .collect::<Result<_, _>>()?,
                on_delete: fk.on_delete,
                match_full: fk.match_full,
            }))
        })
        .collect()
}

/// Return a `set-leakproof` for each built-in function whose leakproofness
/// an administrator changed.
async fn leakproof_statements(
    txn: &Txn,
    catalog: &InMemoryCatalog,
) -> Result<Vec<DdlStatement>, EngineError> {
    let mut builtins = InMemoryCatalog::new(SchemaVersion::new(1));
    install_builtins(&mut builtins).map_err(|e| EngineError::Catalog(e.to_string()))?;
    let mut out = Vec::new();
    for row in read_all(txn, FUNCTIONS, Vec::new()).await? {
        let id = FunctionId::new(id_at(&row, 0)?);
        let (Some(current), Some(default)) =
            (catalog.function_by_id(id), builtins.function_by_id(id))
        else {
            continue;
        };
        if current.properties.leakproof != default.properties.leakproof {
            out.push(DdlStatement::SetLeakproof {
                function: current.name.joined(),
                args: current.args.clone(),
                leakproof: current.properties.leakproof,
            });
        }
    }
    Ok(out)
}

/// Return statements that recreate the user schema on a new database, in
/// an order that satisfies each statement's dependencies: namespaces,
/// principal types, tables with their indexes, entity types, memberships,
/// foreign keys, then function properties.
///
/// Objects in `public` are named unqualified, so the statements are meant
/// to run with the default search path.
///
/// # Errors
///
/// Returns a storage error, or [`EngineError::Catalog`] for a stored
/// catalog that doesn't fit together.
pub async fn schema_statements(txn: &Txn) -> Result<Vec<DdlStatement>, EngineError> {
    let catalog = load_catalog(txn).await?;
    let mut out = Vec::new();
    for (name, owner) in namespace_owners(txn).await? {
        if let Some(owner) = owner {
            out.push(DdlStatement::CreateNamespace { name, owner });
        }
    }
    let mut principal_types = catalog.principal_types();
    principal_types.sort();
    out.extend(
        principal_types
            .into_iter()
            .map(|t| DdlStatement::DeclarePrincipalType(t.as_str().to_owned())),
    );
    let mut tables = catalog.tables();
    tables.retain(|t| !t.system);
    tables.sort_by_key(|t| t.id);
    for table in &tables {
        out.extend(table_statements(table)?);
    }
    out.extend(entity_statements(&catalog)?);
    out.extend(membership_statements(&catalog)?);
    out.extend(foreign_key_statements(&catalog)?);
    out.extend(leakproof_statements(txn, &catalog).await?);
    Ok(out)
}
