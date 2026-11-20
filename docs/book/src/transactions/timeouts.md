# Timeouts and cancellation

## Statement timeouts

`Session::set_statement_timeout(Some(duration))` limits how long each statement may run. `None`, the default, means no limit:

```rust,ignore
session.set_statement_timeout(Some(Duration::from_secs(5)));
```

The timeout applies to every statement the session runs, measured from the moment the statement starts. That covers planning and execution, and for a stream, the time the caller spends between reads (see [Streaming results](streaming.md#timeouts)). A statement still running when its time is up is cancelled:

- it fails with `57014` ("canceling statement due to statement timeout");
- its writes are undone;
- an explicit transaction is aborted, as for any failed statement. In autocommit, the statement's transaction is rolled back.

Cancellation is checked in two places:
- the executor checks the deadline between batches, so a CPU-bound query stops within one batch of it;
- the session also bounds every wait on the executor, so a long wait inside storage is cut off too.

A timeout is a per-session setting. Each session has its own, and changing it affects later statements, including those of an open transaction.

## Cooperative scheduling

Queries run on the Tokio runtime alongside the rest of the application. So that one large query can't monopolize a worker thread, operators that consume their whole input before producing output yield to the scheduler every few input batches. These are hash joins building their table, aggregations, sorts, `nest`, and set operations building their tables. Storage calls run on Interlaken's own storage threads, not the runtime's.

One exception remains. The final sort of a large buffered input is a single computation, and at a few million rows it can hold its thread for a second or more (backlog `sort-yields`). Give latency-sensitive work its own runtime, or more worker threads, if it shares one with large sorts.
