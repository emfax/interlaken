# Metrics and observability

## Storage metrics

`engine.storage().metrics().await?` returns a `StorageMetrics` snapshot:

| Field | Meaning |
| --- | --- |
| `visible_ts` | the commit timestamp new snapshots read at; it rises by one per commit |
| `active_transactions` | open transactions within the maximum age |
| `oldest_snapshot` | the oldest snapshot an open transaction holds, which GC can't pass |
| `expired_transactions` | transactions that exceeded `max_transaction_age` since the database was opened |
| `commit.groups`, `commit.committed`, `commit.conflicts` | commit groups written, transactions committed, and commits rejected with `40001` |
| `commit.validation`, `commit.write` | total time the commit thread spent validating and writing groups |
| `gc.passes`, `gc.failed_passes` | GC passes run and failed |
| `gc.versions_examined`, `gc.versions_removed` | versions GC looked at and removed |
| `gc.last_pass` | the last pass's report |
| `disk_space_bytes` | the database's size on disk |

The counters are cumulative since the database was opened; sample them to compute rates. Things to watch:

- **Conflicts:** `commit.conflicts` rising relative to `commit.committed` means transactions are contending. Retry them, shorten them, or avoid writing hot rows from many transactions.
- **Old snapshots:** a `visible_ts` far ahead of `oldest_snapshot`, or a rising `expired_transactions`, points to long-running transactions holding back GC.
- **Commit cost:** `commit.write` divided by `commit.groups` is the mean cost of a group write, mostly sync latency under `Durability::Sync`.
- **Disk space:** `disk_space_bytes` should level off under a steady workload (see [Garbage collection and disk space](gc.md)).

## Engine metrics

- `Engine::residual_cache_stats()` reports the residual cache's hits and misses. A low hit rate means policies depend on fast-changing context, or principals' data changes often (see [Principals and request context](../security/principals.md#partial-evaluation-and-the-residual-cache)).
- `Engine::memory_pool()` returns the node memory pool, if one is configured, with its `used()`, `peak()` and `limit()` (see [Memory](memory.md)).

## Per-query metrics

`Session::explain(text, true)` runs a statement and reports, per operator, the rows and batches it produced, how many times it ran, its time, storage calls and bytes read for scans, and peak memory for blocking operators (see [EXPLAIN and policy visibility](../security/explain.md)).
