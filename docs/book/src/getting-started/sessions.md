# Sessions and principals

A `Session` runs statements for one principal, the way a connection does in a client-server database. Every query, write, DDL statement and policy change goes through one.

```rust
use interlaken_engine::catalog::{ColumnSpec, TableSpec};
use interlaken_engine::{Engine, EngineOptions};
use interlaken_ir::interp::Datum;
use interlaken_ir::security::{EntityUid, SecurityContext};
use interlaken_ir::text::parse_type;
use interlaken_storage::TxnOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let admin = EntityUid::long("User", 1);
    let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;

    let mut alice = engine.session(SecurityContext::new(admin));
    let spec = TableSpec {
        name: "counters".into(),
        columns: vec![
            ColumnSpec::new("name", parse_type("(not-null text)")?),
            ColumnSpec::new("value", parse_type("(not-null int8)")?),
        ],
        primary_key: vec!["name".into()],
    };
    alice.create_table(&spec).await?;

    // An explicit transaction: both statements commit together, or neither does.
    alice.begin(TxnOptions::default()).await?;
    assert!(alice.in_transaction());
    alice
        .query(r#"(plan (insert counters (values (columns name value) (row "a" 1) (row "b" 2)) (columns name value)))"#, Vec::new())
        .await?;
    alice
        .query(r#"(plan (update counters (filter (get counters :as c) (= c.name "a")) (set value (+ c.value 10)) :as c))"#, Vec::new())
        .await?;
    alice.commit().await?;

    // A rolled-back transaction leaves nothing behind.
    alice.begin(TxnOptions::default()).await?;
    alice.query("(plan (delete counters (get counters :as c) :as c))", Vec::new()).await?;
    alice.rollback()?;

    // Another principal gets its own session. `counters` backs no entity type,
    // so policies don't govern it, and any principal may read it.
    let mut bob = engine.session(SecurityContext::new(EntityUid::long("User", 2)));
    let total = bob
        .query("(plan (aggregate (get counters :as c) (keys) (aggs (agg sum c.value))))", Vec::new())
        .await?;
    assert_eq!(total.rows(), vec![vec![Datum::Int8(13)]]);

    // But bob has no privileges, so may not change the catalog.
    let denied = bob.drop_table("counters").await.unwrap_err();
    assert_eq!(denied.sqlstate(), "42501");
    Ok(())
}
```

## One principal per session

`Engine::session(context)` starts a session for the principal in a `SecurityContext`, with the request facts policies may read: the time, the client's IP address, how the principal authenticated, and any application-defined fields (see [Principals and request context](../security/principals.md)).

The engine doesn't authenticate anyone. The application does, then builds the context, and the engine trusts it. Nothing a client writes in a statement can change the session's principal or context. To act as a different principal, start a different session.

Sessions are cheap: they hold the context and, while one is open, a transaction. Use one session per concurrent task. Many sessions, for the same principal or different ones, can run at once on the same engine; their transactions are isolated from one another (see [Transactions and isolation](../transactions/isolation.md)). A session runs one statement at a time, which its `&mut self` methods enforce.

## What a principal may do

Two separate mechanisms decide:

- **Policies** decide which rows and columns of *governed* tables, those backing an entity type, a principal may read and write. They apply to everyone, administrators included (see [The security model](../security/model.md)).
- **Privileges** decide who may run DDL, change policies, grant privileges, and see policy logic in `EXPLAIN` (see [Privileges](../security/privileges.md)). Without the `admin` privilege, those methods fail with `42501`.

## Transactions

A session is either in autocommit mode or has a transaction open:

- **Autocommit:** with no transaction open, each statement runs in a transaction of its own, which commits if the statement succeeds and rolls back if it fails.
- **Explicit transactions:** `begin(options)` opens a transaction, and later statements run in it until `commit()` or `rollback()`. `TxnOptions` chooses the isolation level, SERIALIZABLE by default, and whether the transaction is read-only. `in_transaction()` says whether one is open.

Every statement is atomic, in either mode. In an explicit transaction, a failed statement's writes are undone, and the transaction then accepts only a rollback, to a savepoint or entirely, as in Postgres (see [Statements, atomicity and savepoints](../transactions/statements.md)). A commit can fail with `40001` if a concurrent transaction conflicted; the application decides whether to retry (see [Retrying](../transactions/isolation.md#retrying)).

DDL and policy changes are transactional too. They can be mixed with queries in one transaction, and are undone by a rollback.

## Settings

- **`set_statement_timeout(Some(duration))`** cancels any statement of this session that runs longer, with `57014` (see [Timeouts and cancellation](../transactions/timeouts.md)).
- **`context()`** returns the session's security context.
- **`privileges()`** returns the privileges its principal holds.

## Streams borrow the session

`Session::query_stream` returns a `QueryStream` that runs the statement as its rows are read. The stream borrows the session mutably, so the session can't run anything else until the stream is finished or dropped, and the compiler enforces this (see [Streaming results](../transactions/streaming.md)). A DML statement whose stream is dropped early still runs to its end, and the session's next call waits for it.
