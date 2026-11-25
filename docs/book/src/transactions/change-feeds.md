# Change feeds and read-your-writes

A program that keeps state derived from its tables (a cache, a routing table, a search index, a copy in another system) needs to know what changed and when. A client that writes on one cluster member and then reads on another needs to see its own write. Both rest on commit timestamps.

## Commit timestamps

Every commit that writes something returns its timestamp, a `CommitTs`:
- `Session::commit` returns `Some(ts)`, or `None` for a transaction that wrote nothing;
- a statement that commits on its own reports it in `QueryResult::commit_ts`, or `QueryStream::commit_ts()` once the stream has ended;
- `Session::snapshot_ts()` returns the snapshot an open transaction reads.

Timestamps are ordered, and every member of a cluster gives a commit the same timestamp. `to_string()` writes one as text, for a header or a cookie, and `parse()` reads it back. Timestamps from different databases can't be compared, and nothing checks that they come from the same one.

## Reading your own writes on another member

A commit returns once the member that ran it has applied it. Another member may not have applied it yet. To read it there, pass its timestamp:
- `Engine::wait_visible(ts, timeout)` waits until this member has applied the commit, or fails with `57014` after `timeout`;
- or begin a read-only transaction with `Freshness::AtLeast(ts)`, which waits the same way, bounded by the session's statement timeout.

Either costs only a wait on the local member. `Freshness::Fresh` (see [Running a cluster](../operations/clusters.md#fresh-and-local-snapshots)) also sees the write, but asks the leader on every transaction. On a single node every commit is visible as soon as it returns.

```rust
# use interlaken_engine::catalog::{ColumnSpec, TableSpec};
# use interlaken_engine::{CommitTs, Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
# use interlaken_storage::{Freshness, TxnOptions};
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin.clone()));
# session.create_table(&TableSpec {
#     name: "orders".into(),
#     columns: vec![
#         ColumnSpec::new("id", parse_type("int8")?),
#         ColumnSpec::new("qty", parse_type("int8")?),
#     ],
#     primary_key: vec!["id".into()],
# }).await?;
let written = session
    .query("(plan (insert orders (values (columns a b) (row 1 5)) (columns id qty)))", Vec::new())
    .await?;
let token = written.commit_ts.expect("the insert committed").to_string();

// Later, perhaps on another member, given only the token:
let ts: CommitTs = token.parse()?;
let mut reader = engine.session(SecurityContext::new(admin));
reader
    .begin(TxnOptions::default().read_only().freshness(Freshness::AtLeast(ts)))
    .await?;
let rows = reader.query("(plan (get orders))", Vec::new()).await?;
assert_eq!(rows.row_count(), 1);
reader.commit().await?;
# Ok(())
# }
```

## Change feeds

`Engine::changes(filter, from)` follows committed changes in commit order. Each `ChangeBatch` is one committed transaction: its `commit_ts` and its changes to the tables the filter selects. A transaction that changed none of them is skipped.

```rust
# use interlaken_engine::catalog::{ColumnSpec, TableSpec};
# use interlaken_engine::{ChangeFilter, ChangeKind, Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));
# session.create_table(&TableSpec {
#     name: "orders".into(),
#     columns: vec![
#         ColumnSpec::new("id", parse_type("int8")?),
#         ColumnSpec::new("qty", parse_type("int8")?),
#     ],
#     primary_key: vec!["id".into()],
# }).await?;
let mut feed = engine.changes(ChangeFilter::tables(["orders"]), None).await?;
session
    .query("(plan (insert orders (values (columns a b) (row 1 5)) (columns id qty)))", Vec::new())
    .await?;
if let Some(batch) = feed.next().await {
    let batch = batch?;
    for change in &batch.changes {
        match change.kind {
            ChangeKind::Insert | ChangeKind::Update => println!("{:?} is now {:?}", change.key, change.row),
            ChangeKind::Delete => println!("{:?} was deleted", change.key),
        }
    }
    println!("handled up to {}", batch.commit_ts); // save it to resume from
}
# Ok(())
# }
```

Each change has:
- `table` and `table_id`;
- `kind`: `Insert`, `Update` or `Delete`. An update that changes a row's primary key is a delete of the old key and an insert of the new one, and a row a transaction inserted and then deleted doesn't appear at all;
- `key`: the primary key values;
- `row`: the new row for inserts and updates, decoded with the table's current columns, so columns added since the change are NULL. `ChangeFilter::keys_only()` leaves rows out.

`ChangeFilter::all()` selects every table except the engine's own.

### Positions and resuming

`from` is exclusive: the feed starts with the first commit after it, or after the last visible commit if `from` is `None`. A program that saves each batch's `commit_ts` after handling it, and passes the saved one as `from` after a restart, sees every commit at least once. `ChangeFeed::position()` is the last commit the feed delivered or passed over.

To start from a consistent copy, read the tables in a read-only transaction, take its `Session::snapshot_ts()`, and follow from there: the feed delivers exactly the changes the copy doesn't have.

Every member of a cluster delivers the same batches with the same timestamps, so a program can resume on another member, as far back as that member's change log reaches.

### Errors

A feed ends after an error; `next()` then returns `None`.
- `55000` means the change log no longer reaches back to the feed's position: the position is older than the log keeps, or the member installed a snapshot that skipped commits. Read the tables again and follow from the new snapshot. The same code means the database keeps no change log.
- `42P01` means a filtered table was dropped, or dropped and created again. `Engine::changes` also returns it for a table that doesn't exist.

### What a feed sees

A feed reads below policies: it reports every change to the tables it follows, whoever made it. It is for the embedding program's own use, not for passing to clients as it is. For clients, use a subscription.

## Subscriptions

`Session::subscribe(table, from)` follows one table's changes as the session's principal may see them. It returns a `Subscription` whose `next()` yields `ChangeBatch`es like a feed's:

```rust,no_run
# async fn run(session: &interlaken_engine::Session) -> Result<(), interlaken_engine::EngineError> {
let mut subscription = session.subscribe("docs", None).await?;
while let Some(batch) = subscription.next().await {
    for change in batch?.changes {
        println!("{:?} {:?} {:?}", change.kind, change.key, change.row);
    }
}
# Ok(())
# }
```

For each committed transaction, the subscription reads the changed rows through the session's policies twice: as they were just before the commit, and just after it, with the policies and entities of that moment.
- A row the principal can read after the commit is an `Update` if it could read it before, and an `Insert` if not, such as a document just shared with them.
- A row it could read before but not after is a `Delete`, whether the row was deleted or the principal lost access to it.
- A row it can read neither before nor after doesn't appear.

Rows carry the principal's column masks. A column the principal is denied without a mask makes the subscription fail with `42501`, as a query reading it would. The subscription runs in a session of its own, with the subscribing session's request context; it doesn't use that session's transaction.

A subscription keeps the row versions it has yet to read from version GC, so a subscription that falls behind holds back GC, and one that is no longer read should be dropped. `from` must also be recent enough that GC hasn't removed versions after it, or `subscribe` fails with `55000`. A position saved from a subscription that is still open, or from a read-only transaction that is still open, is always recent enough.

## Change log retention

Feeds read each member's *change log*, which `StorageOptions::change_retention` sizes:

| Setting | Keeps | Survives a restart | Cost |
| --- | --- | --- | --- |
| `ChangeRetention::Memory { bytes }` (default, 64 MiB) | the most recent changes, up to `bytes` of keys and values | no | memory |
| `ChangeRetention::Durable { age, bytes }` | changes for `age`, up to `bytes` | yes | roughly doubles bytes written per commit |
| `ChangeRetention::Off` | nothing; feeds fail with `55000` | | none |

A durable log is written in the same batch as each commit, so it is exactly as durable as the data. Version GC trims it by age and size. Switching a database from `Durable` to another setting drops its durable log.
