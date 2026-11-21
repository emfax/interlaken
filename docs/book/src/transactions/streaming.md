# Streaming results

`Session::query` collects every row of a result before returning it, which is convenient for small results. For large ones, `Session::query_stream` returns a `QueryStream` that yields Arrow record batches as the statement produces them. The statement executes as batches are read, holding only what its operators need, so a result of any size streams in bounded memory.

```rust
# use interlaken_engine::catalog::{ColumnSpec, TableSpec};
# use interlaken_engine::{Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));
# session.create_table(&TableSpec {
#     name: "docs".into(),
#     columns: vec![
#         ColumnSpec::new("id", parse_type("int8")?),
#         ColumnSpec::new("title", parse_type("(option text)")?),
#     ],
#     primary_key: vec!["id".into()],
# }).await?;
let mut stream = session
    .query_stream("(plan (project (get docs :as d) d.id d.title))", Vec::new())
    .await?;
println!("columns: {:?}", stream.columns());
let mut rows = 0;
while let Some(batch) = stream.next().await {
    let batch = batch?; // an arrow_array::RecordBatch
    rows += batch.num_rows();
}
println!("{rows} rows");
# Ok(())
# }
```

A stream offers:
- `columns()` and `types()`, the output columns' names and IR types, available before any row is read;
- `next()`, the next batch, or `None` at the end;
- `finish()`, which runs the statement to its end and discards the remaining rows;
- `collect()`, which gathers the remaining rows into a `QueryResult`, as `query` does.

`query_plan_stream` does the same for a prepared plan. Streaming 1.5 million rows grows the heap by well under a megabyte; collecting them holds them all.

The stream borrows the session mutably, so the session runs nothing else while the stream is open. The compiler enforces this.

## How a stream ends

A statement ends when its stream does, in one of three ways:

| Ending | Writes | Transaction |
| --- | --- | --- |
| read to the end: `next()` returns `None`, or `finish()` or `collect()` | kept | autocommit commits; a commit failure is returned by the last `next()` instead of `None` |
| an error from `next()`, a timeout included | undone | autocommit rolls back; an explicit transaction is aborted (`25P02` until rolled back) |
| dropped before its end | see below | see below |

After an error, the stream ends: `next()` returns `None`.

## Dropping a stream early

What dropping a stream before its end does depends on whether the statement writes:

- **A query** stops, and is as if it had never run. In autocommit its transaction is rolled back. An explicit transaction isn't aborted, since stopping reading isn't a failure.
- **A DML statement** runs to its end anyway, in the background, and its remaining `returning` rows are discarded. It ends as if it had been read to the end: its writes are kept, and in autocommit it commits even if the session itself is dropped. If it fails, it ends as for an error, aborting an explicit transaction. That failure is reported to no one, but the transaction's `commit` then fails with `25P02`.

This matches Postgres, where a statement's writes complete whether or not the client reads its `RETURNING` rows. A client that reads a few returned rows and moves on can't lose its update.

The session's next call waits for a DML statement still finishing, so the next statement sees its writes. `rollback()` instead stops it, since its writes would be discarded anyway.

## Timeouts

A statement timeout runs on wall-clock time from the `query_stream` call, including the time the caller spends between reads. A slow consumer can therefore time a statement out. See [Timeouts and cancellation](timeouts.md).
