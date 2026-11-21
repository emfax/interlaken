# Transactions and isolation

Every statement runs in a transaction. Without an open transaction, each statement runs in its own, which commits when the statement succeeds: *autocommit*. To group statements, open one explicitly:

```rust
# use interlaken_engine::catalog::{ColumnSpec, TableSpec};
# use interlaken_engine::{Engine, EngineError, EngineOptions, Session};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
use interlaken_storage::{Isolation, TxnOptions};
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));
# session.create_table(&TableSpec {
#     name: "accounts".into(),
#     columns: vec![
#         ColumnSpec::new("id", parse_type("int8")?),
#         ColumnSpec::new("balance", parse_type("int8")?),
#     ],
#     primary_key: vec!["id".into()],
# }).await?;
# session.query("(plan (insert accounts (values (columns id balance) (row 1 100) (row 2 0)) (columns id balance)))", Vec::new()).await?;

/// Move 10 from account 1 to account 2, retrying on serialization failures.
async fn transfer(session: &mut Session) -> Result<(), EngineError> {
    loop {
        session.begin(TxnOptions::new(Isolation::Serializable)).await?;
        let result = async {
            session.query("(plan (update accounts (filter (get accounts :as a) (= a.id 1)) (set balance (- a.balance 10))))", Vec::new()).await?;
            session.query("(plan (update accounts (filter (get accounts :as a) (= a.id 2)) (set balance (+ a.balance 10))))", Vec::new()).await?;
            session.commit().await
        }
        .await;
        match result {
            Err(e) if e.sqlstate() == "40001" => {
                if session.in_transaction() {
                    session.rollback()?;
                }
                // Retry from the start, with a fresh snapshot.
            }
            other => return other,
        }
    }
}
transfer(&mut session).await?;
# Ok(())
# }
```

`begin` takes `TxnOptions`: an isolation level, and `read_only()` for transactions that only read. `commit` makes the transaction's writes durable and visible to transactions that start later. `rollback` discards them. Calling `begin` with a transaction open fails with `25001`; calling `commit` or `rollback` with none open fails with `25P01`.

## Isolation levels

Transactions run optimistically. Each reads a consistent snapshot of committed data plus its own writes, buffers its writes, and is validated when it commits. If a concurrent transaction that committed first conflicts with it, the commit fails with `40001` (serialization failure) and nothing is written.

| Level | Snapshot | Commit fails if a transaction that committed after the snapshot... |
| --- | --- | --- |
| `Serializable` (the default) | one for the whole transaction | wrote a key this one read, or wrote inside a range this one scanned, or wrote a key this one also writes |
| `RepeatableRead` | one for the whole transaction | wrote a key this one also writes (first committer wins) |
| `ReadCommitted` | a fresh one for each statement | wrote a key this one also writes |

- **Serializable** gives true serializability, phantoms included: the result is as if transactions ran one at a time, in commit order. A scan records the range it actually covered, so a concurrent insert into that range conflicts. The check is conservative and can abort transactions that full SSI would let through.
- **Repeatable read** is snapshot isolation. It allows write skew: two transactions each read what the other writes.
- **Read committed** sees each statement's snapshot as of that statement's start. Unlike Postgres, a write that conflicts with a concurrent committed write fails with `40001` rather than waiting for the other transaction and re-checking the row.

Autocommit statements run at `Serializable`.

At every level, some conflicts are always checked:
- with concurrent DDL (see [Tables and column types](../ddl/tables.md#transactions-and-concurrency));
- with concurrent policy activations;
- with concurrent changes to the closure rows that membership maintenance reads.

## Read-only transactions

`TxnOptions::new(isolation).read_only()` opens a transaction that can't write. A write fails with `25006`. Read-only transactions skip commit validation entirely: their snapshot is a consistent point in the commit order, so they never fail with `40001`. Use them for reports and long reads.

## Retrying

The engine never retries a transaction itself. A `40001` means the transaction lost a race and nothing it wrote was kept. Retrying it from the start, with a fresh snapshot, is correct and usually succeeds, as in the example above. `EngineError::sqlstate() == "40001"` identifies these errors, as does `interlaken_storage::Error::is_retryable` for storage errors. A statement that fails for any other reason won't succeed on retry.

Keep transactions short: the longer one runs, the more concurrent commits it can conflict with.

## Limits

| Limit | Setting | When exceeded |
| --- | --- | --- |
| How long a transaction may stay open | `StorageOptions::max_transaction_age`, 5 minutes by default | its next operation fails with `25P04`; versions it needs may have been garbage-collected |
| How much a transaction may write | `StorageOptions::max_write_set_bytes`, 64 MiB of keys and values by default | the write fails with `54000` |

Writes are buffered in memory until commit, which is why the write set is limited. Split large loads into several transactions. See [Configuration](../operations/configuration.md).
