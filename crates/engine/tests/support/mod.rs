//! Shared fixture for engine tests: stored tables matching an IR catalog.

#![allow(dead_code)]

use std::sync::Arc;

use interlaken_engine::policy::{PolicySettings, VerificationMode, create_policy_tables};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::text::parse_catalog;
use interlaken_storage::catalog::{self, ColumnDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum};
use interlaken_storage::{Durability, FjallStorage, Storage, StorageOptions, TxnOptions, rows};

/// The IR catalog matching the stored tables.
pub fn ir_catalog() -> InMemoryCatalog {
    parse_catalog(
        "(catalog
           (table users :id 100 (column id int8) (column dept (option text)) (primary-key id))
           (table groups :id 101 (column id int8) (primary-key id))
           (table memberships :id 102 (column member int8) (column grp int8) (primary-key member grp))
           (table memberships_closure :id 103
             (column member int8) (column grp int8) (column paths int8)
             (primary-key member grp))
           (table docs :id 104 (column id int8) (column owner_id int8) (column secret (option int8)) (primary-key id))
           (entity User :table users :key id (attr dept))
           (entity Group :table groups :key id)
           (entity Doc :table docs :key id (attr owner owner_id :references User) (attr secret))
           (membership User Group :edges memberships (member grp) :closure memberships_closure (member grp paths)))",
    )
    .unwrap()
}

/// Return the IR catalog shared, as provider construction needs it.
pub fn shared_catalog() -> Arc<dyn Catalog> {
    Arc::new(ir_catalog())
}

/// Open storage with users 1 (eng, in group 7) and 2 (in group 8), groups 7 and 8,
/// documents 10 (owned by 1) and 11 (owned by 2, secret 5), and the policy tables in `mode`.
pub async fn setup(dir: &std::path::Path, mode: VerificationMode) -> FjallStorage {
    let storage = FjallStorage::open(
        dir,
        StorageOptions {
            durability: Durability::Buffered,
            gc_interval: None,
            ..StorageOptions::default()
        },
    )
    .unwrap();
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let int = |n: &str| ColumnDef::new(n, ColumnType::Int8);
    let tables = [
        (
            "users",
            vec![int("id"), ColumnDef::new("dept", ColumnType::Text)],
            vec!["id"],
        ),
        ("groups", vec![int("id")], vec!["id"]),
        (
            "memberships",
            vec![int("member"), int("grp")],
            vec!["member", "grp"],
        ),
        (
            "memberships_closure",
            vec![int("member"), int("grp"), int("paths").not_null()],
            vec!["member", "grp"],
        ),
        (
            "docs",
            vec![int("id"), int("owner_id").not_null(), int("secret")],
            vec!["id"],
        ),
    ];
    for (name, columns, pk) in tables {
        let spec = TableSpec {
            name: name.into(),
            columns,
            primary_key: pk.into_iter().map(Into::into).collect(),
        };
        catalog::create_table(&txn, spec).await.unwrap();
    }
    let insert = |table: &'static str, row: Vec<Datum>| {
        let txn = &txn;
        async move {
            let def = catalog::table(txn, table).await.unwrap().unwrap();
            rows::insert(txn, &def, row).await.unwrap();
        }
    };
    insert("users", vec![Datum::Int8(1), Datum::Text("eng".into())]).await;
    insert("users", vec![Datum::Int8(2), Datum::Null]).await;
    insert("groups", vec![Datum::Int8(7)]).await;
    insert("groups", vec![Datum::Int8(8)]).await;
    insert(
        "memberships_closure",
        vec![Datum::Int8(1), Datum::Int8(7), Datum::Int8(1)],
    )
    .await;
    insert(
        "memberships_closure",
        vec![Datum::Int8(2), Datum::Int8(8), Datum::Int8(1)],
    )
    .await;
    insert("docs", vec![Datum::Int8(10), Datum::Int8(1), Datum::Null]).await;
    insert(
        "docs",
        vec![Datum::Int8(11), Datum::Int8(2), Datum::Int8(5)],
    )
    .await;
    create_policy_tables(
        &txn,
        &PolicySettings {
            mode,
            app_context: "{}".into(),
        },
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    storage
}

pub mod session;
