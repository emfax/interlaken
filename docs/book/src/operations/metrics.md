# Metrics and observability

## Telemetry

Interlaken records metrics through the [`metrics`](https://docs.rs/metrics) crate and spans through [`tracing`](https://docs.rs/tracing), and never installs a recorder or subscriber itself: the program embedding it decides where telemetry goes. With nothing installed, recording costs almost nothing.

To collect them, install a `metrics` recorder (a Prometheus or OpenTelemetry exporter, for example) and a `tracing` subscriber when your program starts, then pass Interlaken's metric declarations to the recorder so it can show their units and descriptions:

```rust,ignore
interlaken_storage::telemetry::describe(interlaken_storage::telemetry::METRICS);
```

The `interlaken-telemetry` crate has an in-memory recorder and subscriber for tests (`install_test_recorder`, `install_test_subscriber`).

Some gauges are sampled rather than updated as things change: storage records disk usage, memtable size, flush and compaction backlogs, and transaction counts every `StorageOptions::metrics_interval` (10 seconds by default). `FjallStorage::record_metrics()` records a sample on demand.

Labels never hold keys, values, SQL text or table names, which policies may hide; they take values from fixed sets. Totals of time are gauges that only rise, since `metrics` counters count whole numbers; their names end in `_seconds`, not `_total`, and `rate()` works on them as on counters.

### Spans

| Span | Fields | Covers |
| --- | --- | --- |
| `interlaken.commit` | `isolation`, `durability`, `outcome` | one transaction's commit, from request to answer |
| `interlaken.storage.commit_group` | `transactions`, `bytes` | the sequencer validating and writing one group |
| `interlaken.gc.pass` | `examined`, `removed` | one garbage collection pass |

### Useful ratios

- **Write amplification:** `interlaken_storage_bytes_written_total` divided by `interlaken_storage_bytes_committed_total` is what each committed byte costs in disk writes, including the journal, flushes and compaction. It counts all of the process's disk writes, and is only available on Linux.
- **Conflict rate:** `interlaken_storage_commits_total{outcome="conflict"}` relative to `{outcome="committed"}`.
- **Sequencer saturation:** the rate of `interlaken_storage_sequencer_busy_seconds`, summed over both phases, near 1 means the commit sequencer limits throughput.
- **Garbage collection held back:** a rising `interlaken_storage_oldest_snapshot_age_seconds` means a long transaction is keeping old versions alive.

## Metric reference

<!-- begin generated: metrics -->
### Storage

| Metric | Kind | Unit | Labels | Meaning |
| --- | --- | --- | --- | --- |
| `interlaken_storage_commits_total` | counter | count | `isolation`, `outcome` | Transactions with writes that asked to commit, by isolation level and outcome (`committed`, `conflict`, `error`). |
| `interlaken_storage_commit_duration_seconds` | histogram | seconds | `durability` | Time from a commit request to its answer. |
| `interlaken_storage_bytes_committed_total` | counter | bytes | none | Key and value bytes in committed write sets. |
| `interlaken_storage_commit_group_size` | histogram | count | none | Transactions in each group the commit sequencer writes. |
| `interlaken_storage_sequencer_busy_seconds` | gauge | seconds | `phase` | Cumulative commit sequencer time, by phase (`validation`, `write`). Near one second per second means the sequencer limits commit throughput. |
| `interlaken_storage_gc_versions_total` | counter | count | `outcome` | Versions garbage collection examined and removed (`examined`, `removed`). |
| `interlaken_storage_gc_passes_total` | counter | count | `outcome` | Garbage collection passes, by outcome (`completed`, `failed`). |
| `interlaken_storage_write_throttle_seconds` | gauge | seconds | none | Cumulative time snapshot installs waited for memtables to flush. |
| `interlaken_storage_disk_bytes` | gauge | bytes | `kind` | Bytes on disk, by kind (`data`, `journal`, `other`). Sampled. |
| `interlaken_storage_bytes_written_total` | counter | bytes | none | Bytes the process has written to disk, from `/proc/self/io` (Linux only). Divided by bytes committed, the write amplification. Sampled. |
| `interlaken_storage_memtable_bytes` | gauge | bytes | none | Bytes in fjall's memtables, active and awaiting flush. Sampled. |
| `interlaken_storage_flush_backlog` | gauge | count | none | Memtable flushes queued. Sampled. |
| `interlaken_storage_l0_tables` | gauge | count | none | Tables in level 0 of the data keyspace; a rising count means compaction is behind. Sampled. |
| `interlaken_storage_compaction_seconds` | gauge | seconds | none | Cumulative time fjall has spent compacting. Sampled. |
| `interlaken_storage_active_transactions` | gauge | count | none | Open transactions within the maximum age. Sampled. |
| `interlaken_storage_expired_transactions_total` | counter | count | none | Transactions that exceeded the maximum transaction age. Sampled. |
| `interlaken_storage_oldest_snapshot_age_seconds` | gauge | seconds | none | Age of the oldest open transaction's snapshot, which holds back garbage collection. Sampled. |

<!-- end generated: metrics -->

## Storage metrics snapshot

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
