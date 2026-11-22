//! Helpers for tests through the engine's public API.

use interlaken_engine::catalog::{
    AttributeSpec, ColumnSpec, EntitySpec, IndexKey, IndexSpec, MembershipSpec, TableSpec,
};
use interlaken_engine::policy::{PolicyDocument, PolicyLanguage};
use interlaken_engine::{Engine, EngineError, EngineOptions, Session};
use interlaken_exec::{from_stored, to_stored};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::Datum;
use interlaken_ir::security::{Effect, EntityUid, Policy, SecurityContext, print_sec_expr};
use interlaken_storage::rows::{self, RowScan};
use interlaken_storage::{Durability, Storage, StorageOptions, TxnOptions, catalog};

/// The database's first administrator.
pub fn admin() -> EntityUid {
    EntityUid::str("Admin", "root")
}

/// Return engine options with buffered durability and no background GC.
pub fn options() -> EngineOptions {
    EngineOptions {
        storage: StorageOptions {
            durability: Durability::Buffered,
            gc_interval: None,
            ..StorageOptions::default()
        },
        ..EngineOptions::default()
    }
}

/// Create a database in `dir` administered by [`admin`]: on one node, or,
/// with the `cluster` feature and `INTERLAKEN_TEST_CLUSTER` set, on a
/// three-member cluster, returning a follower ([`super::cluster`]).
pub async fn create(dir: &std::path::Path) -> Engine {
    #[cfg(feature = "cluster")]
    if std::env::var_os("INTERLAKEN_TEST_CLUSTER").is_some() {
        return super::cluster::create(dir).await;
    }
    Engine::create(dir, options(), &admin()).await.unwrap()
}

/// Create a single-node database in `dir`, even in cluster mode: for tests
/// that restart it with [`Engine::open`].
pub async fn create_single(dir: &std::path::Path) -> Engine {
    Engine::create(dir, options(), &admin()).await.unwrap()
}

/// Start a session for `principal` with empty request facts.
pub fn session(engine: &Engine, principal: EntityUid) -> Session {
    engine.session(SecurityContext::new(principal))
}

/// Return the type parsed from its text form.
pub fn ty(text: &str) -> interlaken_ir::Type {
    interlaken_ir::text::parse_type(text).unwrap()
}

/// Return a table spec from `(name, type)` columns and primary key names.
pub fn table(name: &str, columns: &[(&str, &str)], primary_key: &[&str]) -> TableSpec {
    TableSpec {
        name: name.into(),
        columns: columns
            .iter()
            .map(|(n, t)| ColumnSpec::new(*n, ty(t)))
            .collect(),
        primary_key: primary_key.iter().map(|k| (*k).to_owned()).collect(),
    }
}

/// Create, through engine DDL, every table, index, entity type and
/// membership of an IR catalog, so the engine's catalog matches it by name.
pub async fn install(admin: &mut Session, ir: &InMemoryCatalog) {
    for def in &ir.tables() {
        let name = |c: interlaken_ir::ColumnId| def.column_by_id(c).unwrap().name.to_string();
        admin
            .create_table(&TableSpec {
                name: def.name.last().to_string(),
                columns: def
                    .columns
                    .iter()
                    .map(|c| ColumnSpec::new(c.name.as_str(), c.ty.clone()))
                    .collect(),
                primary_key: def.primary_key.iter().map(|c| name(*c)).collect(),
            })
            .await
            .unwrap();
        for index in &def.indexes {
            admin
                .create_index(&IndexSpec {
                    name: index.name.to_string(),
                    table: def.name.last().to_string(),
                    columns: index
                        .columns
                        .iter()
                        .map(|k| IndexKey {
                            column: name(k.column),
                            descending: k.descending,
                            nulls_first: k.nulls_first,
                        })
                        .collect(),
                    unique: index.unique,
                    nones_distinct: true,
                })
                .await
                .unwrap();
        }
    }
    let table_name = |id| ir.table_by_id(id).unwrap().name.last().to_string();
    let column_name = |table, column| {
        ir.table_by_id(table)
            .unwrap()
            .column_by_id(column)
            .unwrap()
            .name
            .to_string()
    };
    for entity in ir.entities() {
        admin
            .declare_entity(&EntitySpec {
                name: entity.name.to_string(),
                table: table_name(entity.table),
                key: column_name(entity.table, entity.key),
                attributes: entity
                    .attributes
                    .iter()
                    .map(|a| AttributeSpec {
                        name: a.name.to_string(),
                        column: column_name(entity.table, a.column),
                        references: a.references.as_ref().map(ToString::to_string),
                    })
                    .collect(),
            })
            .await
            .unwrap();
    }
    for entity in ir.entities() {
        for m in ir.memberships_of(&entity.name) {
            admin
                .declare_membership(&MembershipSpec {
                    member: m.member.to_string(),
                    group: m.group.to_string(),
                    edges: table_name(m.edges),
                    edge_columns: (
                        column_name(m.edges, m.edge_columns.0),
                        column_name(m.edges, m.edge_columns.1),
                    ),
                    closure: table_name(m.closure),
                    closure_columns: (
                        column_name(m.closure, m.closure_columns.0),
                        column_name(m.closure, m.closure_columns.1),
                        column_name(m.closure, m.closure_columns.2),
                    ),
                })
                .await
                .unwrap();
        }
    }
}

/// Replace the rows of each named table with the given rows, directly in
/// storage (bypassing policies), in one committed transaction.
pub async fn replace_rows(engine: &Engine, tables: &[(&str, Vec<Vec<Datum>>)]) {
    let txn = engine.storage().begin(TxnOptions::default()).await.unwrap();
    for (name, new_rows) in tables {
        let def = catalog::table(&txn, name).await.unwrap().unwrap();
        let mut scan = RowScan::all();
        let mut keys = Vec::new();
        loop {
            let batch = rows::scan(&txn, &def, &scan).await.unwrap();
            for row in &batch.rows {
                keys.push(
                    def.primary_key()
                        .iter()
                        .map(|&k| row[k].clone())
                        .collect::<Vec<_>>(),
                );
            }
            match batch.resume {
                Some(resume) => scan.resume = Some(resume),
                None => break,
            }
        }
        for key in keys {
            rows::delete(&txn, &def, &key).await.unwrap();
        }
        for row in new_rows {
            let stored = row.iter().map(|d| to_stored(d).unwrap()).collect();
            rows::insert(&txn, &def, stored).await.unwrap();
        }
    }
    txn.commit().await.unwrap();
}

/// Return every row of the named table in primary key order, read
/// directly from storage (bypassing policies and masks).
pub async fn stored_rows(engine: &Engine, name: &str) -> Vec<Vec<Datum>> {
    let txn = engine.storage().begin(TxnOptions::default()).await.unwrap();
    let def = catalog::table(&txn, name).await.unwrap().unwrap();
    let mut scan = RowScan::all();
    let mut out = Vec::new();
    loop {
        let batch = rows::scan(&txn, &def, &scan).await.unwrap();
        out.extend(
            batch
                .rows
                .iter()
                .map(|row| row.iter().map(from_stored).collect::<Vec<_>>()),
        );
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => break,
        }
    }
    txn.abort();
    out
}

/// Propose, verify and activate an RLS policy set as `admin`, confirming
/// anything the gates flag; return the new policy version.
pub async fn activate_rls(admin: &mut Session, policies: &str) -> Result<i64, EngineError> {
    let document = PolicyDocument {
        language: PolicyLanguage::Rls,
        body: policies.into(),
    };
    let id = admin.submit_policies(&document, "").await?;
    admin.verify_policies(id).await?;
    admin.activate_policies(id, true).await
}

/// Render policies in the RLS text form, with their columns and masks.
pub fn rls_text(policies: &[Policy]) -> String {
    let items: Vec<String> = policies
        .iter()
        .map(|p| {
            let effect = match p.effect {
                Effect::Permit => "permit",
                Effect::Forbid => "forbid",
            };
            let actions: Vec<&str> = p.actions.iter().map(|a| a.name()).collect();
            let column = p
                .column
                .as_ref()
                .map_or_else(String::new, |c| format!(" :column {c}"));
            let mask = p
                .mask
                .as_ref()
                .map_or_else(String::new, |m| format!(" :mask {}", m.name()));
            format!(
                "({effect} ({}) {} {}{column}{mask})",
                actions.join(" "),
                p.resource_type,
                print_sec_expr(&p.condition)
            )
        })
        .collect();
    format!("(policies {})", items.join(" "))
}
