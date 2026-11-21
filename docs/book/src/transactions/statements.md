# Statements, atomicity and savepoints

## Each statement is atomic

A statement either takes full effect or none: if it fails partway, after writing some rows, every write it made is undone. Errors of every kind work this way, including a constraint violation, an expression error, a write check, a timeout and running out of memory.

What happens next depends on the transaction:

- **In autocommit**, the statement's own transaction is rolled back, and the session is ready for the next statement.
- **In an explicit transaction**, the transaction becomes *aborted*, as in Postgres. Every later statement fails with `25P02` until the transaction is rolled back, entirely or to a savepoint. `commit` of an aborted transaction rolls it back and fails with `25P02`, so nothing from it is kept.

`Session::is_aborted()` reports whether the open transaction is aborted. It is `async` because it first waits for any DML statement still finishing in the background (see [Streaming results](streaming.md#dropping-a-stream-early)).

## Savepoints

A savepoint marks a point in a transaction that can be rolled back to without abandoning the whole transaction. Rolling back to a savepoint undoes everything since it was opened and clears the aborted state:

```rust
# use interlaken_engine::catalog::{ColumnSpec, TableSpec};
# use interlaken_engine::{Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
# use interlaken_storage::TxnOptions;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));
# session.create_table(&TableSpec {
#     name: "users".into(),
#     columns: vec![
#         ColumnSpec::new("id", parse_type("int8")?),
#         ColumnSpec::new("name", parse_type("text")?),
#     ],
#     primary_key: vec!["id".into()],
# }).await?;
session.begin(TxnOptions::default()).await?;
session.query(r#"(plan (insert users (values (columns id name) (row 1 "ada")) (columns id name)))"#, Vec::new()).await?;

let before_bob = session.savepoint().await?;
let duplicate = session
    .query(r#"(plan (insert users (values (columns id name) (row 1 "bob")) (columns id name)))"#, Vec::new())
    .await;
assert_eq!(duplicate.unwrap_err().sqlstate(), "23505");
assert!(session.is_aborted().await?);

session.rollback_to(before_bob).await?;
assert!(!session.is_aborted().await?);
session.query(r#"(plan (insert users (values (columns id name) (row 2 "bob")) (columns id name)))"#, Vec::new()).await?;
session.commit().await?; // ada and bob (as user 2) are committed
# Ok(())
# }
```

| Method | Does |
| --- | --- |
| `savepoint()` | opens a savepoint at the current state and returns its ID; fails with `25P02` in an aborted transaction |
| `rollback_to(id)` | undoes everything since the savepoint, including later savepoints, and clears the aborted state; the savepoint stays open, so it can be rolled back to again |
| `release(id)` | closes the savepoint, keeping its writes; fails with `25P02` in an aborted transaction |

Savepoints nest. Rolling back to or releasing a savepoint closes the savepoints opened after it. They are all `async`, for the same reason as `is_aborted`.

## What a statement sees

Within a transaction:
- **Earlier statements:** each statement sees the writes of the earlier ones.
- **Its own writes:** a statement never sees its own writes in what it reads. This keeps DML from re-reading rows it has just written (see [What a statement sees](../ir/dml.md#what-a-statement-sees)).
- **Constraint checks:** the checks do see the statement's own writes, so two rows of one insert with the same key conflict.

Under `ReadCommitted`, each statement also takes a fresh snapshot of other transactions' committed writes when it starts. Under the other levels, the snapshot is the transaction's first.
