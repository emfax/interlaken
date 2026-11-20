# Your first database

This chapter creates a database, adds a table, writes and reads rows, and opens the database again. The example is complete: `cargo test` compiles and runs it.

```rust
use interlaken_engine::catalog::{ColumnSpec, TableSpec};
use interlaken_engine::{Engine, EngineOptions};
use interlaken_ir::interp::Datum;
use interlaken_ir::security::{EntityUid, SecurityContext};
use interlaken_ir::text::parse_type;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let admin = EntityUid::long("User", 1);

    {
        // Create the database, with `admin` as its first administrator.
        let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
        let mut session = engine.session(SecurityContext::new(admin.clone()));

        session
            .create_table(&TableSpec {
                name: "notes".into(),
                columns: vec![
                    ColumnSpec::new("id", parse_type("(not-null int8)")?),
                    ColumnSpec::new("body", parse_type("(not-null text)")?),
                    ColumnSpec::new("pinned", parse_type("bool")?),
                ],
                primary_key: vec!["id".into()],
            })
            .await?;

        let inserted = session
            .query(
                r#"(plan (insert notes
                     (values (columns id body pinned)
                       (row 1 "buy milk" true)
                       (row 2 "call the bank" null)
                       (row 3 "book flights" false))
                     (columns id body pinned)))"#,
                Vec::new(),
            )
            .await?;
        assert_eq!(inserted.row_count(), 3);

        // Parameters are passed separately from the text, as `(param N TYPE)`.
        let result = session
            .query(
                "(plan (sort (filter (get notes :as n) (>= n.id (param 1 int8)))
                         (asc n.id)))",
                vec![Datum::Int8(2)],
            )
            .await?;
        assert_eq!(result.columns, ["id", "body", "pinned"]);
        assert_eq!(
            result.rows(),
            vec![
                vec![Datum::Int8(2), Datum::Text("call the bank".into()), Datum::Null],
                vec![Datum::Int8(3), Datum::Text("book flights".into()), Datum::Bool(false)],
            ]
        );
    } // Dropping the engine closes the database.

    // Everything was committed, so it is all there after reopening.
    let engine = Engine::open(dir.path(), EngineOptions::default()).await?;
    let mut session = engine.session(SecurityContext::new(admin.clone()));
    let result = session
        .query("(plan (aggregate (get notes :as n) (keys) (aggs (agg count))))", Vec::new())
        .await?;
    assert_eq!(result.rows(), vec![vec![Datum::Int8(3)]]);

    // A directory holds one database: creating it again fails.
    drop(engine);
    assert!(Engine::create(dir.path(), EngineOptions::default(), &admin).await.is_err());
    Ok(())
}
```

## Creating and opening

`Engine::create(path, options, admin)` creates a database in the directory `path`, and fails if one is already there. The database starts with:

- no tables;
- `admin` holding the `admin` privilege, which allows DDL, policy changes and granting privileges (see [Privileges](../security/privileges.md));
- an empty Cedar policy set, which denies everything. It doesn't matter yet: policies govern only tables that back an *entity type*, and `notes` backs none (see [The security model](../security/model.md#governed-tables)).

`Engine::open(path, options)` opens an existing database, and fails if `path` holds none. The catalog, data, privileges and policies are all stored in the database, so nothing needs to be set up again. `EngineOptions` holds the settings that apply while the database is open, such as memory budgets and durability (see [Configuration](../operations/configuration.md)).

An `Engine` is cheap to clone, and its clones share everything. Open a database once per process and share the engine between tasks. Dropping the last clone closes the database.

## Tables

`Session::create_table` takes a `TableSpec`: a name, columns with their IR types, and a primary key, which every table needs. A type is written in the [IR text form](../ir/types.md), so `(not-null int8)` is a non-NULL 64-bit integer and `text` a text column that allows NULL. Primary key columns are NOT NULL whether or not their type says so. See [Tables and column types](../ddl/tables.md) for the types a table can hold, and for adding columns, dropping tables and indexes.

## Statements

Queries and writes are plans in the [IR text language](../ir/syntax.md), wrapped in `(plan ...)`:

- **`get`** reads a table, and `:as` names it for later references, so `n.id` is the `id` column of `notes`.
- **`filter`**, **`sort`**, **`project`**, **`aggregate`**, **`join`** and the other [relational operators](../ir/operators.md) take relations and return relations.
- **`insert`**, **`update`** and **`delete`** are operators too, and return the rows they wrote (see [Inserting, updating and deleting](../ir/dml.md)).

`Session::query(text, params)` runs a statement and collects its result. Each statement runs in its own transaction unless the session has one open (see [Sessions and principals](sessions.md)).

## Results

A `QueryResult` has:

| Field or method | What it is |
| --- | --- |
| `columns` | the output column names; unnamed computed columns are `?column?` |
| `types` | the output columns' IR types |
| `batches` | the rows, as Arrow `RecordBatch`es |
| `rows()` | the rows as `Vec<Vec<Datum>>`, in order |
| `row_count()` | the number of rows |

`Datum` is a value of any IR type: `Datum::Int8`, `Datum::Text`, `Datum::Bool`, `Datum::Null` and so on. `rows()` is convenient for small results. Code that processes many rows can read the Arrow batches directly, and for results too large to hold, `Session::query_stream` returns them a batch at a time (see [Streaming results](../transactions/streaming.md)).

A failed statement returns an `EngineError`, whose `sqlstate()` gives its SQLSTATE code (see [Error codes](../reference/errors.md)).
