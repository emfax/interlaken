# Tables and column types

Tables are created and changed through `Session` methods, not IR text. DDL is transactional: each operation runs as one statement, in the open transaction or in its own, and commits or rolls back with the transaction like any other write. Only principals with the `admin` privilege can run DDL; anyone else gets `42501`. The principal that created the database is its first administrator (see [Privileges](../security/privileges.md)).

## Creating a table

```rust
# use interlaken_engine::catalog::{ColumnSpec, TableSpec};
# use interlaken_engine::{Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
use interlaken_ir::text::parse_type;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));

session
    .create_table(&TableSpec {
        name: "docs".into(),
        columns: vec![
            ColumnSpec::new("id", parse_type("(not-null int8)")?),
            ColumnSpec::new("title", parse_type("text")?),
            ColumnSpec::new("tags", parse_type("(array (not-null text))")?),
        ],
        primary_key: vec!["id".into()],
    })
    .await?;
# Ok(())
# }
```

A `TableSpec` has:

- **`name`:**
  - It must not be empty.
  - Names starting with `sys_` or `engine_` are reserved for the catalog.
  - Names are case-sensitive.
- **`columns`:** between 1 and 1,600 columns, in order, each a name and an IR type (see [Types](../ir/types.md)). Column names must be distinct. A type without `(not-null ...)` allows NULLs.
- **`primary_key`:**
  - One or more of the columns, in key order. Every table has a primary key.
  - Primary key columns become NOT NULL whatever their declared type.
  - Arrays and `jsonb` can't be key columns.

Types the engine can't store fail with `42P16`: records, entity references and arrays of arrays. A duplicate table name fails with `42P07`.

Rows are stored in primary key order, and primary key lookups and ranges are the cheapest reads (see [Indexes](indexes.md)).

## Adding a column

```rust,ignore
session.add_column("docs", &ColumnSpec::new("summary", parse_type("text")?)).await?;
```

The new column goes at the end, and existing rows read it as NULL:
- An added column must be nullable, since there are no column defaults to fill existing rows; adding a NOT NULL column fails with `42P16`.
- A column name that already exists fails with `42701`.

There is no way yet to drop, rename or retype a column, or to rename a table.

## Dropping a table

```rust,ignore
session.drop_table("docs").await?;
```

This drops the table with its rows and indexes:
- A table backing an entity type or holding a membership can't be dropped; remove those declarations first (see [Entities and memberships](entities.md)).
- Dropping a table that doesn't exist fails with `42P01`.

## Transactions and concurrency

DDL can be mixed with queries in one transaction, and the transaction sees its own changes at once:

```rust,ignore
session.begin(TxnOptions::default()).await?;
session.create_table(&spec).await?;
session.query("(plan (insert notes (values (columns id) (row 1)) (columns id)))", Vec::new()).await?;
session.commit().await?; // or rollback(), which undoes both
```

Every DDL statement changes the catalog's *schema version*. This has three effects:

- **Prepared plans go stale.** A plan prepared before the change fails with `0A000` and must be prepared again (see [Prepared plans](../ir/parameters.md#prepared-plans)).
- **Concurrent writers conflict.** A transaction that ran DDL conflicts with every concurrent transaction that wrote anything, and one of them fails at commit with `40001`. The conflict is broader than it needs to be: DDL on one table conflicts with writes to another. Run DDL when write traffic is light.
- **Readers keep their snapshot.** A read-only transaction already running keeps seeing the catalog as of its snapshot.

## What the catalog records

Each table is recorded in storage's `sys_tables` and `sys_columns`, and its columns' IR types in `engine_columns` (see [Catalog tables](../reference/catalog-tables.md)). Clients can't read these tables through queries.

The on-disk format isn't stable yet: databases are not migrated between Interlaken versions.
