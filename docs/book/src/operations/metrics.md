# Metrics and observability

## Telemetry

Interlaken records metrics through the [`metrics`](https://docs.rs/metrics) crate and spans through [`tracing`](https://docs.rs/tracing), and never installs a recorder or subscriber itself: the program embedding it decides where telemetry goes. With nothing installed, recording costs almost nothing.

To collect them, install a `metrics` recorder (a Prometheus or OpenTelemetry exporter, for example) and a `tracing` subscriber when your program starts, then pass Interlaken's metric declarations to the recorder so it can show their units and descriptions:

```rust,ignore
interlaken_engine::describe_metrics();
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
| `interlaken.raft.propose` | `transactions` | on the leader, one group of commits from proposal to commit and apply |
| `interlaken.raft.snapshot.send`, `interlaken.raft.snapshot.install` | none | sending a snapshot to a replica, and installing a received one |
| `interlaken.statement` | `kind`, `outcome`; for queries and DML also `rows_scanned`, `rows_returned`, `bytes_read`, `bytes_written`, `cpu_seconds`, `peak_memory` | one statement, from its start to its end |
| `interlaken.parse`, `interlaken.plan` | none | parsing and planning a query or DML statement, under its statement span |
| `interlaken.execute` | none | entered each time the statement's plan is polled for rows, under its statement span |

No span records SQL text or IR, so a host that wants statement text in its traces adds it in its own span around the call, where it decides what is safe to record.

## Statement usage

Every query and DML statement counts the work it caused, whether or not a recorder is installed, as a `Usage`:

| Field | Meaning |
| --- | --- |
| `rows_scanned` | rows storage returned to the statement's scans and lookups |
| `rows_returned` | rows the statement returned |
| `bytes_read` | bytes of the values its scans and lookups read, as decoded |
| `bytes_written` | key and value bytes it added to its transaction's write set; zero if it failed or was cancelled |
| `cpu` | time spent executing on the statement's task, not counting waits for storage or the caller |
| `peak_memory` | the most query memory it reserved at once |
| `elapsed` | time from its start to its end |

`QueryResult::usage` carries a collected statement's usage, and `QueryStream::usage()` a streamed one's once it has ended. `Session::transaction_usage()` sums the statements of the open transaction, or of the last one; a statement outside a transaction is its own transaction. Applications can log, bill or set quotas on it without a metrics backend.

### Cost attribution

`EngineOptions::attribution` adds each query and DML statement's usage to the `interlaken_usage_*` metrics under an attribution key, so you can see which workloads cost what:

- `Attribution::Off`, the default: the `interlaken_usage_*` metrics aren't recorded.
- `Attribution::Principal`: the session's principal, such as `User::"42"`.
- `Attribution::Context("tenant")`: the session's application context field of that name (`context.app.tenant`). A session without it counts as `none`.

The key is chosen when a session starts. At most `EngineOptions::attribution_limit` distinct keys (1,000 by default) are kept; sessions with new keys beyond it count as `other`, and `interlaken_usage_attribution_overflow_total` counts them. With many principals, attribute by a coarser context field such as a tenant or plan tier, or bill from `Usage` directly.

Attribution keys are stored in your metrics system, with whatever access control it has: turn on `Principal` only if principal IDs may be seen there.

### Useful ratios

- **Write amplification:** `interlaken_storage_bytes_written_total` divided by `interlaken_storage_bytes_committed_total` is what each committed byte costs in disk writes, including the journal, flushes and compaction. It counts all of the process's disk writes, and is only available on Linux.
- **Conflict rate:** `interlaken_storage_commits_total{outcome="conflict"}` relative to `{outcome="committed"}`.
- **Sequencer saturation:** the rate of `interlaken_storage_sequencer_busy_seconds`, summed over both phases, near 1 means the commit sequencer limits throughput.
- **Network cost:** on a cluster, the rate of `interlaken_net_bytes_total` (per peer) divided by that of `interlaken_storage_bytes_committed_total` on the leader is how many bytes cross the network for each committed byte. Where peers sit in other zones or regions, that ratio drives most of the bill. Replication's gauges (`interlaken_raft_*`) carry a `node` label, and follower lag is reported by the leader.
- **Wasted scanning:** `interlaken_rows_returned_total` divided by `interlaken_rows_scanned_total`. A low ratio means filters aren't reaching storage, or an index is missing.
- **Garbage collection held back:** a rising `interlaken_storage_oldest_snapshot_age_seconds` means a long transaction is keeping old versions alive.

## Metric reference

<!-- begin generated: metrics -->
### Statements

| Metric | Kind | Unit | Labels | Meaning |
| --- | --- | --- | --- | --- |
| `interlaken_statements_total` | counter | count | `kind`, `outcome` | Statements run, by kind (`select`, `insert`, `update`, `delete`, `unplanned` for a query that failed before planning finished, `ddl`, `policy`, `admin`, `other`) and outcome (`ok`, `error`, `cancelled`). |
| `interlaken_statement_duration_seconds` | histogram | seconds | `kind`, `phase` | Time per statement phase (`parse`, `plan`, `execute`). A streamed statement's execution includes time the caller spends between batches. |
| `interlaken_statement_cpu_seconds` | histogram | seconds | `kind` | Time each query or DML statement spent executing on its task, not counting waits for storage. |
| `interlaken_rows_scanned_total` | counter | count | none | Rows storage returned to scans and lookups. |
| `interlaken_rows_returned_total` | counter | count | none | Rows statements returned. Divided by rows scanned, how much scanning was useful. |
| `interlaken_bytes_read_total` | counter | bytes | none | Bytes of the values scans and lookups read, as decoded. |
| `interlaken_bytes_written_total` | counter | bytes | none | Key and value bytes statements added to their transactions' write sets. |
| `interlaken_query_memory_peak_bytes` | histogram | bytes | none | The most query memory each query or DML statement reserved at once. |
| `interlaken_query_memory_exceeded_total` | counter | count | none | Statements that failed for exceeding a memory budget (SQLSTATE `53200`). |
| `interlaken_authz_denials_total` | counter | count | `kind` | Statements refused by security, by kind: `column` (a column policy), `write` (a write check), `statement` (a missing privilege or another security error). Rows policies filter out aren't denials. |
| `interlaken_usage_statements_total` | counter | count | `attribution` | Query and DML statements, by attribution key. Recorded only with `EngineOptions::attribution` on. |
| `interlaken_usage_rows_scanned_total` | counter | count | `attribution` | Rows scanned, by attribution key. Recorded only with attribution on. |
| `interlaken_usage_bytes_read_total` | counter | bytes | `attribution` | Bytes read from storage, by attribution key. Recorded only with attribution on. |
| `interlaken_usage_bytes_written_total` | counter | bytes | `attribution` | Bytes added to write sets, by attribution key. Recorded only with attribution on. |
| `interlaken_usage_cpu_seconds` | gauge | seconds | `attribution` | Cumulative executor time, by attribution key. Recorded only with attribution on. |
| `interlaken_usage_attribution_overflow_total` | counter | count | none | Sessions whose attribution key was new when the limit of distinct keys (`EngineOptions::attribution_limit`) was already reached; their usage counts under `other`. |
| `interlaken_policy_proposals_total` | counter | count | `step`, `outcome` | Policy proposal steps (`submit`, `verify`, `activate`) by outcome (`ok`, `error`). |

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

### Replication

| Metric | Kind | Unit | Labels | Meaning |
| --- | --- | --- | --- | --- |
| `interlaken_raft_term` | gauge | count | `node` | The current Raft term. |
| `interlaken_raft_is_leader` | gauge | count | `node` | 1 on the leader, 0 elsewhere. |
| `interlaken_raft_leader_changes_total` | counter | count | `node` | Times this replica saw a new leader. |
| `interlaken_raft_commit_index` | gauge | count | `node` | The index of the last log entry known committed. |
| `interlaken_raft_applied_index` | gauge | count | `node` | The index of the last log entry applied here. |
| `interlaken_raft_follower_lag_entries` | gauge | count | `node`, `peer` | On the leader: how many entries each other member's log trails the leader's by. |
| `interlaken_raft_proposal_duration_seconds` | histogram | seconds | none | On the leader: time from proposing a group of commits to Raft until it is committed and applied. |
| `interlaken_raft_proposal_size` | histogram | count | none | On the leader: transactions in each proposal. |
| `interlaken_raft_reads_total` | counter | count | `kind` | Fresh-read requests answered, by path: `lease` or `read_index` on the leader, `forwarded` from a follower to the leader. |
| `interlaken_raft_snapshots_total` | counter | count | `direction`, `outcome` | Snapshot transfers, by direction (`sent`, `received`) and outcome (`ok`, `error`). |
| `interlaken_raft_snapshot_bytes_total` | counter | bytes | `direction` | Snapshot bytes sent and received. |
| `interlaken_raft_snapshot_duration_seconds` | histogram | seconds | `direction` | Time per snapshot transfer, from its first chunk to the end of sending, or of installing. |

### Network

| Metric | Kind | Unit | Labels | Meaning |
| --- | --- | --- | --- | --- |
| `interlaken_net_bytes_total` | counter | bytes | `peer`, `direction` | Bytes of the calls this replica makes to each peer: requests `sent` and responses `received`, as encoded, without gRPC and TLS framing. Divided by bytes committed, the network cost of each committed byte. |
| `interlaken_net_requests_total` | counter | count | `peer`, `kind`, `outcome` | Calls this replica makes to each peer, by kind (`append`, `vote`, `pre_vote`, `commit`, `read_index`, `snapshot_chunk`, `transfer_leader`, `membership`) and outcome (`ok`; `not_delivered`, surely not received; `no_reply`, perhaps received). |
| `interlaken_net_served_bytes_total` | counter | bytes | `direction` | Bytes of the requests this replica answers: requests `received` and responses `sent`, as encoded. |
| `interlaken_net_served_total` | counter | count | `kind` | Requests this replica answered, by kind. |

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
