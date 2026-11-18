//! Catalog, DDL and row layer behaviour (ADR-0007, milestone M4).

use interlaken_storage::catalog::{self, ColumnDef, IndexSpec, TableDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum, SortOrder};
use interlaken_storage::rows::{self, RowScan};
use interlaken_storage::{
    CatalogError, Conflict, ConstraintViolation, Durability, Error, FjallStorage, Isolation,
    Storage, StorageOptions, Txn, TxnOptions,
};

/// Open storage in a fresh directory with buffered durability.
fn open(dir: &tempfile::TempDir) -> FjallStorage {
    let options = StorageOptions {
        pool_threads: 2,
        durability: Durability::Buffered,
        ..StorageOptions::default()
    };
    FjallStorage::open(dir.path(), options).unwrap()
}

/// Begin a read-write transaction at `isolation`.
async fn begin(storage: &FjallStorage, isolation: Isolation) -> Txn {
    storage.begin(TxnOptions::new(isolation)).await.unwrap()
}

/// The `users` table: id int8 pk, email text, team int4, note text.
fn users_spec() -> TableSpec {
    TableSpec {
        name: "users".into(),
        columns: vec![
            ColumnDef::new("id", ColumnType::Int8),
            ColumnDef::new("email", ColumnType::Text),
            ColumnDef::new("team", ColumnType::Int4).not_null(),
            ColumnDef::new("note", ColumnType::Text),
        ],
        primary_key: vec!["id".into()],
    }
}

/// Construct a `users` row.
fn user(id: i64, email: Option<&str>, team: i32) -> Vec<Datum> {
    vec![
        Datum::Int8(id),
        email.map_or(Datum::Null, |e| Datum::Text(e.into())),
        Datum::Int4(team),
        Datum::Null,
    ]
}

/// Create `users` with a unique index on email and a non-unique index on team; commit.
async fn create_users(storage: &FjallStorage) -> TableDef {
    let txn = begin(storage, Isolation::Serializable).await;
    catalog::create_table(&txn, users_spec()).await.unwrap();
    catalog::create_index(&txn, index("users_email", "email", true))
        .await
        .unwrap();
    let def = catalog::create_index(&txn, index("users_team", "team", false))
        .await
        .unwrap();
    txn.commit().await.unwrap();
    def
}

/// Construct a one-column index spec on `users`.
fn index(name: &str, column: &str, unique: bool) -> IndexSpec {
    IndexSpec {
        name: name.into(),
        table: "users".into(),
        columns: vec![(column.into(), SortOrder::ASC)],
        unique,
    }
}

/// Return the table definition read in `txn`.
async fn users(txn: &Txn) -> TableDef {
    catalog::table(txn, "users").await.unwrap().unwrap()
}

/// Return every row of `def`, in primary key order.
async fn all_rows(txn: &Txn, def: &TableDef) -> Vec<Vec<Datum>> {
    let mut scan = RowScan::all();
    scan.limit_rows = 2;
    let mut out = Vec::new();
    loop {
        let batch = rows::scan(txn, def, &scan).await.unwrap();
        out.extend(batch.rows);
        match batch.resume {
            Some(resume) => scan.resume = Some(resume),
            None => return out,
        }
    }
}

/// Assert `def`'s indexes match its rows in `txn`.
async fn assert_consistent(txn: &Txn, def: &TableDef) {
    let problems = rows::check_indexes(txn, def).await.unwrap();
    assert!(problems.is_empty(), "index problems: {problems:?}");
}

#[tokio::test]
/// Assert inserts, reads, scans, updates and deletes keep indexes in step.
async fn row_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let storage = open(&dir);
    create_users(&storage).await;
    let txn = begin(&storage, Isolation::Serializable).await;
    let def = users(&txn).await;
    for (id, email, team) in [(3, "c@x", 1), (1, "a@x", 2), (2, "b@x", 1)] {
        rows::insert(&txn, &def, user(id, Some(email), team))
            .await
            .unwrap();
    }
    txn.next_statement();
    assert_eq!(
        rows::get(&txn, &def, &[Datum::Int8(1)]).await.unwrap(),
        Some(user(1, Some("a@x"), 2))
    );
    let ids: Vec<Datum> = all_rows(&txn, &def)
        .await
        .iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(ids, vec![Datum::Int8(1), Datum::Int8(2), Datum::Int8(3)]);

    let team = def.index("users_team").unwrap();
    let batch = rows::scan_index(&txn, &def, team, &RowScan::prefix(vec![Datum::Int4(1)]))
        .await
        .unwrap();
    assert_eq!(
        batch.primary_keys,
        vec![vec![Datum::Int8(2)], vec![Datum::Int8(3)]]
    );

    // Change the primary key, email and team of row 2.
    assert!(
        rows::update(&txn, &def, &[Datum::Int8(2)], user(20, Some("b2@x"), 3))
            .await
            .unwrap()
    );
    assert!(rows::delete(&txn, &def, &[Datum::Int8(3)]).await.unwrap());
    assert!(!rows::delete(&txn, &def, &[Datum::Int8(3)]).await.unwrap());
    assert!(
        !rows::update(&txn, &def, &[Datum::Int8(99)], user(99, None, 1))
            .await
            .unwrap()
    );
    txn.next_statement();
    assert_consistent(&txn, &def).await;
    let email = def.index("users_email").unwrap();
    let batch = rows::scan_index(
        &txn,
        &def,
        email,
        &RowScan::prefix(vec![Datum::Text("b2@x".into())]),
    )
    .await
    .unwrap();
    assert_eq!(batch.primary_keys, vec![vec![Datum::Int8(20)]]);
    txn.commit().await.unwrap();

    let txn = begin(&storage, Isolation::Serializable).await;
    let def = users(&txn).await;
    assert_eq!(
        all_rows(&txn, &def).await,
        vec![user(1, Some("a@x"), 2), user(20, Some("b2@x"), 3)]
    );
    assert_consistent(&txn, &def).await;
}

#[tokio::test]
/// Assert primary key, unique, NOT NULL and type checks, including within one statement.
async fn constraints() {
    let dir = tempfile::tempdir().unwrap();
    let storage = open(&dir);
    create_users(&storage).await;
    let txn = begin(&storage, Isolation::RepeatableRead).await;
    let def = users(&txn).await;
    rows::insert(&txn, &def, user(1, Some("a@x"), 1))
        .await
        .unwrap();

    // Same statement: the first insert is not yet visible to reads, but still collides.
    let err = rows::insert(&txn, &def, user(1, Some("z@x"), 1))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Constraint(ConstraintViolation::PrimaryKey { .. })
    ));
    let err = rows::insert(&txn, &def, user(2, Some("a@x"), 1))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Constraint(ConstraintViolation::Unique { index }) if index == "users_email")
    );
    let err = rows::insert(
        &txn,
        &def,
        vec![Datum::Int8(3), Datum::Null, Datum::Null, Datum::Null],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, Error::Constraint(ConstraintViolation::NotNull { column, .. }) if column == "team")
    );
    let err = rows::insert(
        &txn,
        &def,
        vec![Datum::Int4(3), Datum::Null, Datum::Int4(1), Datum::Null],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Row(_)));

    // NULLs never collide in a unique index.
    rows::insert(&txn, &def, user(4, None, 1)).await.unwrap();
    rows::insert(&txn, &def, user(5, None, 1)).await.unwrap();

    // Updates check only values that change.
    rows::insert(&txn, &def, user(6, Some("b@x"), 1))
        .await
        .unwrap();
    let err = rows::update(&txn, &def, &[Datum::Int8(6)], user(6, Some("a@x"), 1))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Constraint(ConstraintViolation::Unique { .. })
    ));
    let err = rows::update(&txn, &def, &[Datum::Int8(6)], user(1, Some("b@x"), 1))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Constraint(ConstraintViolation::PrimaryKey { .. })
    ));
    rows::update(&txn, &def, &[Datum::Int8(6)], user(6, Some("b@x"), 9))
        .await
        .unwrap();
    // Freeing a value lets another row take it in the same statement.
    rows::delete(&txn, &def, &[Datum::Int8(1)]).await.unwrap();
    rows::insert(&txn, &def, user(7, Some("a@x"), 1))
        .await
        .unwrap();
    txn.next_statement();
    assert_consistent(&txn, &def).await;
    txn.commit().await.unwrap();
}

#[tokio::test]
/// Assert DDL is transactional: aborted DDL leaves no trace and dropped tables lose their data.
async fn ddl_is_transactional() {
    let dir = tempfile::tempdir().unwrap();
    let storage = open(&dir);
    let txn = begin(&storage, Isolation::Serializable).await;
    let def = catalog::create_table(&txn, users_spec()).await.unwrap();
    rows::insert(&txn, &def, user(1, None, 1)).await.unwrap();
    txn.abort();

    let txn = begin(&storage, Isolation::Serializable).await;
    assert!(catalog::table(&txn, "users").await.unwrap().is_none());
    assert!(catalog::tables(&txn).await.unwrap().is_empty());
    drop(txn);

    let def = create_users(&storage).await;
    let txn = begin(&storage, Isolation::Serializable).await;
    rows::insert(&txn, &def, user(1, Some("a@x"), 1))
        .await
        .unwrap();
    txn.commit().await.unwrap();

    let txn = begin(&storage, Isolation::Serializable).await;
    catalog::drop_table(&txn, "users").await.unwrap();
    assert!(catalog::table(&txn, "users").await.unwrap().is_none());
    // Recreate under the same name in the same transaction: a new, empty table.
    let recreated = catalog::create_table(&txn, users_spec()).await.unwrap();
    assert_ne!(recreated.id(), def.id());
    catalog::create_index(&txn, index("users_email", "email", true))
        .await
        .unwrap();
    txn.commit().await.unwrap();

    let txn = begin(&storage, Isolation::Serializable).await;
    let def = users(&txn).await;
    assert!(all_rows(&txn, &def).await.is_empty());
    assert_eq!(def.indexes().len(), 1);
    assert_consistent(&txn, &def).await;
    let err = catalog::create_table(&txn, users_spec()).await.unwrap_err();
    assert!(matches!(err, Error::Catalog(CatalogError::TableExists(_))));
    let err = catalog::drop_index(&txn, "users_team").await.unwrap_err();
    assert!(matches!(err, Error::Catalog(CatalogError::NoSuchIndex(_))));
}

#[tokio::test]
/// Assert index backfill, unique backfill failure, DROP INDEX and ADD COLUMN.
async fn index_and_column_changes() {
    let dir = tempfile::tempdir().unwrap();
    let storage = open(&dir);
    let txn = begin(&storage, Isolation::Serializable).await;
    let def = catalog::create_table(&txn, users_spec()).await.unwrap();
    for id in 0..10 {
        let email = format!("{}@x", id % 3);
        rows::insert(&txn, &def, user(id, Some(&email), (id % 2) as i32))
            .await
            .unwrap();
    }
    txn.commit().await.unwrap();

    let txn = begin(&storage, Isolation::Serializable).await;
    let err = catalog::create_index(&txn, index("users_email", "email", true))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Constraint(ConstraintViolation::Unique { .. })
    ));
    txn.abort();

    let txn = begin(&storage, Isolation::Serializable).await;
    let def = catalog::create_index(&txn, index("users_email", "email", false))
        .await
        .unwrap();
    assert_eq!(def.schema_version(), 2);
    assert_consistent(&txn, &def).await;
    let def = catalog::add_column(&txn, "users", ColumnDef::new("age", ColumnType::Int2))
        .await
        .unwrap();
    assert_eq!(def.columns().len(), 5);
    let err = catalog::add_column(&txn, "users", ColumnDef::new("age", ColumnType::Int2))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Catalog(CatalogError::ColumnExists { .. })
    ));
    let err = catalog::add_column(
        &txn,
        "users",
        ColumnDef::new("x", ColumnType::Int2).not_null(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Catalog(CatalogError::InvalidDefinition(_))
    ));
    txn.commit().await.unwrap();

    let txn = begin(&storage, Isolation::Serializable).await;
    let def = users(&txn).await;
    let old = rows::get(&txn, &def, &[Datum::Int8(1)])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.len(), 5);
    assert_eq!(
        old[4],
        Datum::Null,
        "rows written before ADD COLUMN read it as NULL"
    );
    let mut new_row = user(100, Some("new@x"), 1);
    new_row.push(Datum::Int2(42));
    rows::insert(&txn, &def, new_row.clone()).await.unwrap();
    let def = catalog::drop_index(&txn, "users_email").await.unwrap();
    assert!(def.indexes().is_empty());
    txn.next_statement();
    assert_eq!(
        rows::get(&txn, &def, &[Datum::Int8(100)]).await.unwrap(),
        Some(new_row)
    );
    txn.commit().await.unwrap();
}

#[tokio::test]
/// Assert the catalog and rows survive a restart.
async fn catalog_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let storage = open(&dir);
        let def = create_users(&storage).await;
        let txn = begin(&storage, Isolation::Serializable).await;
        rows::insert(&txn, &def, user(1, Some("a@x"), 1))
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }
    let storage = open(&dir);
    let txn = begin(&storage, Isolation::Serializable).await;
    let def = users(&txn).await;
    assert_eq!(def.indexes().len(), 2);
    assert_eq!(all_rows(&txn, &def).await, vec![user(1, Some("a@x"), 1)]);
    assert_consistent(&txn, &def).await;
    // The ID counter continues rather than reusing IDs.
    let other = catalog::create_table(
        &txn,
        TableSpec {
            name: "other".into(),
            ..users_spec()
        },
    )
    .await
    .unwrap();
    assert!(other.id() > def.id());
}

#[tokio::test]
/// Assert invalid definitions and reserved names are rejected.
async fn invalid_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let storage = open(&dir);
    let txn = begin(&storage, Isolation::Serializable).await;
    for spec in [
        TableSpec {
            name: "sys_mine".into(),
            ..users_spec()
        },
        TableSpec {
            name: String::new(),
            ..users_spec()
        },
        TableSpec {
            primary_key: vec![],
            ..users_spec()
        },
        TableSpec {
            primary_key: vec!["id".into(), "id".into()],
            ..users_spec()
        },
        TableSpec {
            columns: vec![
                ColumnDef::new("id", ColumnType::Int8),
                ColumnDef::new("id", ColumnType::Int8),
            ],
            ..users_spec()
        },
    ] {
        let err = catalog::create_table(&txn, spec.clone()).await.unwrap_err();
        assert!(
            matches!(err, Error::Catalog(CatalogError::InvalidDefinition(_))),
            "{spec:?}: {err}"
        );
    }
    let err = catalog::create_table(
        &txn,
        TableSpec {
            primary_key: vec!["missing".into()],
            ..users_spec()
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Catalog(CatalogError::NoSuchColumn { .. })
    ));
    assert!(catalog::table(&txn, "sys_tables").await.unwrap().is_none());
    let read_only = storage
        .begin(TxnOptions::default().read_only())
        .await
        .unwrap();
    assert!(matches!(
        catalog::create_table(&read_only, users_spec()).await,
        Err(Error::ReadOnlyTransaction)
    ));
}

#[tokio::test]
/// Assert DML under a stale definition aborts when DDL on the table commits first, at every level.
async fn dml_conflicts_with_concurrent_ddl() {
    for isolation in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let storage = open(&dir);
        let txn = begin(&storage, Isolation::Serializable).await;
        catalog::create_table(&txn, users_spec()).await.unwrap();
        txn.commit().await.unwrap();

        let dml = begin(&storage, isolation).await;
        let def = users(&dml).await;
        rows::insert(&dml, &def, user(1, Some("a@x"), 1))
            .await
            .unwrap();
        let ddl = begin(&storage, isolation).await;
        catalog::create_index(&ddl, index("users_email", "email", true))
            .await
            .unwrap();
        ddl.commit().await.unwrap();
        assert!(
            matches!(
                dml.commit().await,
                Err(Error::SerializationFailure(Conflict::DependencyChanged))
            ),
            "{isolation:?}"
        );
    }
}

#[tokio::test]
/// Assert an index backfill aborts if DML on the table committed after its snapshot, at every level.
async fn backfill_conflicts_with_concurrent_dml() {
    for isolation in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let storage = open(&dir);
        let txn = begin(&storage, Isolation::Serializable).await;
        catalog::create_table(&txn, users_spec()).await.unwrap();
        txn.commit().await.unwrap();

        let ddl = begin(&storage, isolation).await;
        let dml = begin(&storage, isolation).await;
        let def = users(&dml).await;
        rows::insert(&dml, &def, user(1, Some("a@x"), 1))
            .await
            .unwrap();
        dml.commit().await.unwrap();
        catalog::create_index(&ddl, index("users_email", "email", true))
            .await
            .unwrap();
        assert!(
            matches!(
                ddl.commit().await,
                Err(Error::SerializationFailure(Conflict::DependencyChanged))
            ),
            "{isolation:?}"
        );

        let check = begin(&storage, Isolation::Serializable).await;
        let def = users(&check).await;
        assert!(def.indexes().is_empty());
        assert_eq!(all_rows(&check, &def).await.len(), 1);
    }
}

#[tokio::test]
/// Assert concurrent CREATE TABLE with one name lets exactly one commit.
async fn concurrent_create_same_name() {
    let dir = tempfile::tempdir().unwrap();
    let storage = open(&dir);
    let a = begin(&storage, Isolation::ReadCommitted).await;
    let b = begin(&storage, Isolation::ReadCommitted).await;
    catalog::create_table(&a, users_spec()).await.unwrap();
    catalog::create_table(&b, users_spec()).await.unwrap();
    a.commit().await.unwrap();
    assert!(matches!(
        b.commit().await,
        Err(Error::SerializationFailure(Conflict::WriteWrite))
    ));
}

#[tokio::test]
/// Assert READ COMMITTED DML still conflicts with DDL that committed before a snapshot refresh.
///
/// The insert's dependency on the table definition was registered at the
/// first statement's snapshot; refreshing the snapshot must not hide the
/// `CREATE INDEX` that committed in between.
async fn read_committed_dml_dependency_survives_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let storage = open(&dir);
    let txn = begin(&storage, Isolation::Serializable).await;
    catalog::create_table(&txn, users_spec()).await.unwrap();
    txn.commit().await.unwrap();

    let dml = begin(&storage, Isolation::ReadCommitted).await;
    let def = users(&dml).await;
    rows::insert(&dml, &def, user(1, Some("a@x"), 1))
        .await
        .unwrap();
    let ddl = begin(&storage, Isolation::Serializable).await;
    catalog::create_index(&ddl, index("users_email", "email", true))
        .await
        .unwrap();
    ddl.commit().await.unwrap();
    dml.next_statement();
    assert!(matches!(
        dml.commit().await,
        Err(Error::SerializationFailure(Conflict::DependencyChanged))
    ));
    let check = begin(&storage, Isolation::Serializable).await;
    assert_consistent(&check, &users(&check).await).await;
}

#[tokio::test]
/// Assert a READ COMMITTED backfill still conflicts with DML that committed before a snapshot refresh.
async fn read_committed_ddl_dependency_survives_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let storage = open(&dir);
    let txn = begin(&storage, Isolation::Serializable).await;
    catalog::create_table(&txn, users_spec()).await.unwrap();
    txn.commit().await.unwrap();

    let ddl = begin(&storage, Isolation::ReadCommitted).await;
    catalog::create_index(&ddl, index("users_email", "email", true))
        .await
        .unwrap();
    let dml = begin(&storage, Isolation::Serializable).await;
    let def = users(&dml).await;
    rows::insert(&dml, &def, user(1, Some("a@x"), 1))
        .await
        .unwrap();
    dml.commit().await.unwrap();
    ddl.next_statement();
    assert!(matches!(
        ddl.commit().await,
        Err(Error::SerializationFailure(Conflict::DependencyChanged))
    ));
}
