---
id: 0039
title: Telemetry and Cost Attribution
status: accepted  # proposal | accepted | implemented | superseded
tags: [observability, storage, executor, replication, engine]
feature-flag:
created: 2026-11-24
accepted: 2026-11-24
implemented:
spec-sections: []
superseded-by:
---

# ADR-0039: Telemetry and Cost Attribution

## Status

Accepted. Phases 1 (facades and storage), 2 (statements and `Usage`), 3 (replication and the network) and 4 (attribution; its planner feedback waits for ADR-0036) are built; its *As implemented* notes record where the result differs from this design.

## Summary

Interlaken measures little of what it does, and only in pieces:

- Storage keeps cumulative counters for commits ([`CommitStats`]) and version garbage collection ([`GcStats`]).
- The executor counts rows, time, storage calls and bytes per operator, but only for `EXPLAIN ANALYZE` (ADR-0028).
- openraft tracks its own state, which the replication crate reads only to find the leader.

There are no spans, no metrics library and no way to export anything. [ADR-0036](0036-statistics-and-cost-based-planning.md) plans a metric for estimate quality on "ADR-0033's metrics", which were never defined.

This ADR decides:

1. **Two facades:** the libraries record with [`tracing`] (spans and events) and [`metrics`] (counters, gauges and histograms), and never install an exporter. Recording with nothing installed costs almost nothing.
2. **What to measure** in storage, the executor, the engine and replication, with fixed names and labels.
3. **Usage per statement.** Every statement counts the work it caused (rows, bytes read and written, CPU time, memory, replication traffic) and returns it with its result as a `Usage` value. This works without any metrics backend.
4. **Cost attribution.** Usage is also recorded as metrics labelled by a bounded *attribution key* (the principal, or an `app.tenant` context value), so you can see which workloads cost what.
5. **Planner feedback.** Estimated and actual rows are recorded per operator, closing the loop ADR-0036 needs.
6. **An optional telemetry crate,** `interlaken-telemetry`, that a program embedding Interlaken can use to export to Prometheus or OpenTelemetry.

## Context

### Two meanings of cost

**Running cost** is what operating Interlaken costs in resources, and in the cloud in money. Providers bill for:

- bytes written to disk (IOPS, throughput and SSD wear);
- bytes stored;
- bytes sent between zones and regions;
- CPU time and memory.

ADR-0038 was a running-cost decision, though not described as one. Full stored snapshots cost disk writes and network transfers in proportion to the stored history. The figures behind it came from one-off tests, because nothing in a running system measures bytes written or sent.

**Planning cost** is the planner's estimate of a plan's work (ADR-0036), used to pick between plans. It is only as good as its row estimates, and nothing yet records how wrong they are in practice.

Both need the same data: the work done, counted where it happens (bytes, rows, time), then added up by statement, by plan operator, by node or by workload.

### What exists

| Where | What | Limits |
| --- | --- | --- |
| `interlaken-storage` | `CommitStats`: commit groups, commits, conflicts, sequencer validation and write time | Cumulative only; polled by benchmarks |
| `interlaken-storage` | `GcStats`: passes, failures, versions examined and removed | Same |
| `interlaken-exec` | `NodeMetrics` per operator: loops, rows, batches, time, storage calls, bytes read, peak memory | Only under `EXPLAIN ANALYZE` |
| `interlaken-exec` | Query memory budgets (ADR-0027) | Reserve and fail; nothing reported |
| `interlaken-replication` | openraft's `RaftMetrics` watch: term, leader, indexes, membership | Read only to find the leader |

Interlaken is a set of libraries. The engine is embedded, and there is no server binary yet. Whatever program embeds it decides where telemetry goes.

### Constraints

- **No exporter in the libraries.** A library that installs a global recorder or subscriber conflicts with its host's.
- **Metrics must not leak data.** Interlaken enforces row and column policies (ADR-0015 to ADR-0020). A label holding a key, a literal from SQL or an error message with a value in it would bypass them. Labels hold only identifiers drawn from fixed sets, or attribution keys the operator has opted into.
- **Bounded label sets.** Each label combination is a separate time series in Prometheus. Labels must not grow with the data, the number of queries or the number of users without a limit.
- **The hot path stays fast.** ADR-0033 tuned the executor batch by batch. Instrumentation counts per batch or per operation, never per row, and spans cover operations, not batches.
- **Exact counters stay testable.** Tests and benchmarks rely on `CommitStats` and `GcStats` being exact and available without a recorder. They stay.

## Decision

### 1. Facades, not exporters

The libraries depend on `tracing` and `metrics` only:

- **`tracing`** for spans: a statement, its planning and execution, a commit group, a garbage-collection pass, a snapshot transfer, a Raft proposal. Events record what an operator would want in a log, such as a leader change, a failed garbage-collection pass, an install, or a throttle that hit its 2 s limit.
- **`metrics`** for counters, gauges and histograms, under the names in section 3.

Neither installs anything. With no subscriber or recorder installed, a span or metric call is a check of a global and returns at once.

Existing typed counters stay as they are. Where a metric duplicates one (commits, conflicts, versions removed), the code increments both. The typed counter is the exact value for tests; the metric is for monitoring.

### 2. Names and labels

- **Names** are `interlaken_<area>_<what>_<unit>`, with counters ending in `_total`, following Prometheus conventions. For example: `interlaken_storage_bytes_written_total`, `interlaken_query_duration_seconds`.
- **Units** are seconds and bytes, never milliseconds or megabytes.
- **Histograms** use fixed buckets per kind: latency buckets from 100 µs to 60 s, size buckets from 1 KiB to 1 GiB.
- **Labels** come from fixed sets: `isolation`, `durability`, `operator`, `phase`, `outcome`, `peer` (a node ID), `kind`. The one open label is `attribution` (section 5), which is off by default and capped.
- **Never labels:** keys, values, SQL text, plan fingerprints derived from literals, table or column names (these can be sensitive under policy), or error messages.

Every metric is declared once in a `metrics` module in the crate that records it, with its description and unit. A generated reference section of the book (`UPDATE_BOOK=1`) lists them all, so the names are part of the documented interface.

### 3. What to measure

#### Storage

| Metric | Kind | Labels | Meaning |
| --- | --- | --- | --- |
| `interlaken_storage_commits_total` | counter | `isolation`, `outcome` (`committed`, `conflict`, `error`) | Transactions finished |
| `interlaken_storage_commit_duration_seconds` | histogram | `durability` | From commit request to acknowledgement |
| `interlaken_storage_commit_group_size` | histogram | | Transactions per group |
| `interlaken_storage_sequencer_busy_seconds_total` | counter | `phase` (`validation`, `write`) | Mirrors `CommitStats` |
| `interlaken_storage_bytes_committed_total` | counter | | Key and value bytes in committed write sets |
| `interlaken_storage_bytes_written_total` | counter | | Bytes fjall wrote to disk (journal, flushes, compaction) |
| `interlaken_storage_disk_bytes` | gauge | `kind` (`data`, `journal`, `other`) | Bytes on disk |
| `interlaken_storage_memtable_bytes` | gauge | | fjall's memtable backlog |
| `interlaken_storage_compaction_backlog` | gauge | | Segments waiting to compact |
| `interlaken_storage_write_throttle_seconds_total` | counter | | Time writers waited in the memtable throttle (ADR-0038) |
| `interlaken_storage_oldest_snapshot_age_seconds` | gauge | | Age of the oldest open read snapshot, which holds back garbage collection |
| `interlaken_storage_gc_versions_total` | counter | `outcome` (`examined`, `removed`) | Mirrors `GcStats` |
| `interlaken_storage_gc_passes_total` | counter | `outcome` (`completed`, `failed`) | Same |

`bytes_written / bytes_committed` is the write amplification: what each committed byte costs in disk writes, including MVCC, the journal and compaction. Where fjall doesn't expose a figure, it is sampled from the file system on a timer, not on the commit path.

#### Queries

| Metric | Kind | Labels | Meaning |
| --- | --- | --- | --- |
| `interlaken_statements_total` | counter | `kind` (`select`, `insert`, `update`, `delete`, `ddl`, `explain`), `outcome` | Statements run |
| `interlaken_statement_duration_seconds` | histogram | `kind`, `phase` (`parse`, `plan`, `execute`) | Time per phase |
| `interlaken_rows_scanned_total` | counter | | Rows storage returned to scans |
| `interlaken_rows_returned_total` | counter | | Rows statements returned |
| `interlaken_bytes_read_total` | counter | | Value bytes scans read (from `NodeMetrics`) |
| `interlaken_query_memory_peak_bytes` | histogram | | Peak reservation per query (ADR-0027) |
| `interlaken_query_memory_exceeded_total` | counter | | Queries refused for their memory budget |
| `interlaken_authz_denials_total` | counter | `kind` (`row`, `column`, `write`, `statement`) | Policy denials (no table names) |
| `interlaken_policy_proposals_total` | counter | `outcome` | Policy change proposals (ADR-0030) |
| `interlaken_udf_duration_seconds` | histogram | | Time in WASM UDFs, once ADR-0029 is built |

`rows_returned / rows_scanned` shows how much scanning is wasted. A low ratio means filters aren't reaching storage, or an index is missing.

#### Planner feedback

Under `EXPLAIN ANALYZE`, operators already report actual rows. With this ADR, each statement's executor also records, for its top-level operators, the estimated and actual rows. The **q-error**, `max(est/actual, actual/est)` with zeros treated as one, goes to `interlaken_planner_q_error` (a histogram labelled by `operator`). This is the metric ADR-0036 planned. A rising q-error shows stale statistics before they show up as slow plans.

To keep this cheap, it counts rows per operator at the batch level, which the executor does anyway for `LIMIT` and metrics. It records the q-error once per statement.

#### Replication

| Metric | Kind | Labels | Meaning |
| --- | --- | --- | --- |
| `interlaken_raft_term` | gauge | | Current term |
| `interlaken_raft_is_leader` | gauge | | 1 on the leader |
| `interlaken_raft_leader_changes_total` | counter | | Leader changes seen |
| `interlaken_raft_commit_index`, `interlaken_raft_applied_index` | gauge | | Log positions |
| `interlaken_raft_follower_lag_entries` | gauge | `peer` | On the leader: entries each follower is behind |
| `interlaken_raft_proposal_duration_seconds` | histogram | | From proposal to commit |
| `interlaken_raft_reads_total` | counter | `kind` (`read_index`, `lease`, `local`) | Reads by consistency path |
| `interlaken_raft_snapshots_total` | counter | `direction` (`sent`, `received`), `outcome` | Snapshot transfers |
| `interlaken_raft_snapshot_bytes_total` | counter | `direction` | Snapshot bytes moved |
| `interlaken_raft_snapshot_duration_seconds` | histogram | `direction` | Transfer and install time |
| `interlaken_net_bytes_total` | counter | `peer`, `direction` | Bytes over the transport, per peer |
| `interlaken_net_requests_total` | counter | `peer`, `kind`, `outcome` | Transport calls (`append`, `vote`, `snapshot_chunk`, ...) |

The Raft gauges come from openraft's `RaftMetrics` watch: a task follows the watch and updates the gauges when it changes. `peer` is a node ID; cluster size bounds it.

`net_bytes_total` divided by `bytes_committed_total` gives the network bytes per committed byte. In a cluster spread across zones, that ratio is most of the bill.

### 4. Usage per statement

Every statement collects a `Usage` value as it runs:

```rust
/// The work one statement caused.
pub struct Usage {
    /// Rows storage returned to scans.
    pub rows_scanned: u64,
    /// Rows the statement returned or changed.
    pub rows_out: u64,
    /// Value bytes read from storage.
    pub bytes_read: u64,
    /// Key and value bytes the statement's commit wrote (zero if read-only).
    pub bytes_written: u64,
    /// Time on executor threads, not counting waits.
    pub cpu: Duration,
    /// The most query memory reserved at once.
    pub peak_memory: u64,
    /// Wall-clock time from start to result.
    pub elapsed: Duration,
}
```

`QueryResult` and the results of DML statements carry it, and an explicit transaction adds up its statements. Its counts come from the counters the executor already keeps under `EXPLAIN ANALYZE`: those are summed at the root rather than per operator, so this costs no more than counting rows does.

`Usage` is the cost signal that works without a metrics backend. An application can log it per request, bill on it, or enforce quotas on it, without running Prometheus. Replication traffic isn't in it: one commit's share of a batched append is ill-defined. Network cost is attributed in total by node (section 3).

`cpu` is measured per poll of the statement's stream on the executor threads, so time blocked waiting on storage or on the commit sequencer is left out. `elapsed` includes it.

### 5. Cost attribution

Each statement's `Usage` is also added to counters labelled by an **attribution key**:

- `interlaken_usage_rows_scanned_total{attribution}`
- `interlaken_usage_bytes_read_total{attribution}`
- `interlaken_usage_bytes_written_total{attribution}`
- `interlaken_usage_cpu_seconds_total{attribution}`
- `interlaken_usage_statements_total{attribution}`

The key comes from an `Attribution` setting on the engine:

- **`Off`** (the default): the counters have no `attribution` label. Totals only.
- **`Principal`**: the session's principal ID (ADR-0031).
- **`Context(name)`**: a request context value such as `app.tenant`, which the security provider already receives (ADR-0015).

The number of distinct keys is capped (default 1,000). Keys beyond the cap are counted under `other`, and `interlaken_usage_attribution_overflow_total` counts statements that fell into it. The cap keeps a deployment with millions of principals from making millions of series. Applications with that many should use `Context` with a coarser key, such as a tenant or plan tier, or bill from `Usage` directly.

Attribution keys are identifiers the operator chose to expose. The documentation says so: turning on `Principal` puts principal IDs in the metrics store, with whatever access control that store has.

### 6. Spans

Spans are named `interlaken.<area>.<operation>`:

- `interlaken.statement` (fields: `kind`; the attribution key if attribution is on), with children `interlaken.parse`, `interlaken.plan`, `interlaken.execute`;
- `interlaken.commit` (fields: `isolation`, `durability`, `outcome`), linked to the commit group that wrote it;
- `interlaken.storage.commit_group` (fields: transactions, bytes);
- `interlaken.gc.pass`, `interlaken.raft.propose`, `interlaken.raft.snapshot.send`, `interlaken.raft.snapshot.install`.

Operators don't get their own spans: they would cost per poll. `EXPLAIN ANALYZE` remains the tool for operator detail. The `Usage` of a statement is recorded on its span when it ends, so a tracing backend shows each statement's cost beside its timing.

Span fields follow the label rules: no SQL text, keys or values. A host that wants SQL text in its traces can add it from its own span around the call, where it decides what is safe.

### 7. The telemetry crate

`interlaken-telemetry` is an optional crate for programs that embed Interlaken. It doesn't depend on the engine. It provides:

- `install_prometheus(addr)`: a Prometheus recorder with Interlaken's histogram buckets, serving `/metrics`.
- `install_otlp(endpoint)`: OpenTelemetry export of spans and metrics.
- `install_test_recorder()`: a recorder that captures metrics in memory, for tests that check a metric moved.

No other crate depends on it. Tests use its test recorder as a dev-dependency.

## Testing

- **Metrics move as expected.** With the test recorder installed, tests run a commit, a conflict, a garbage-collection pass, a query, a denial and a snapshot transfer, and check the counters and histograms that should change.
- **Typed counters and metrics agree.** After a mixed workload, the commit and garbage-collection metrics equal `CommitStats` and `GcStats`.
- **`Usage` is right.** For queries with known answers, `rows_scanned`, `rows_out` and `bytes_read` match `EXPLAIN ANALYZE`'s root and scan counts, and `bytes_written` matches the commit's write set.
- **No data in labels.** A test runs statements whose keys, values and literals contain a marker string, then checks that no recorded label value or span field contains it.
- **Bounded labels.** Attribution with more than the cap of distinct keys produces at most cap + 1 series, and counts the overflow.
- **Cost of instrumentation.** The executor and commit benchmarks (ADR-0033, `commit_throughput`) run with no recorder, and with the Prometheus recorder installed. Without a recorder, throughput must stay within 2% of the current figures; with one, within 5%.

## Implementation Phases

### Phase 1: facades and storage

- Add `tracing` and `metrics` to the workspace, and `interlaken-telemetry` with the test recorder.
- Declare and record the storage metrics: commits, durations, group sizes, sequencer time, bytes committed, garbage collection, the write throttle and the oldest snapshot's age.
- Sample disk usage, memtable size and compaction backlog on a timer.
- Add the `interlaken.commit`, `interlaken.storage.commit_group` and `interlaken.gc.pass` spans.
- Add the generated metric reference to the book.

#### As implemented (phase 1)

- **Time totals are gauges.** `metrics` counters count whole numbers, so cumulative times (`interlaken_storage_sequencer_busy_seconds`, `interlaken_storage_write_throttle_seconds`, `interlaken_storage_compaction_seconds`) are gauges that only rise, incremented by fractional seconds. Their names drop `_total`; `rate()` works on them as on counters, though Prometheus can't detect their reset on restart as it does for counters.
- **Bytes written come from the operating system.** fjall reports sizes but not bytes written, so `interlaken_storage_bytes_written_total` is the process's `write_bytes` from `/proc/self/io`, sampled. It counts all of the process's writes and exists only on Linux.
- **Compaction backlog is two gauges.** fjall doesn't report segments waiting to compact. `interlaken_storage_flush_backlog` (memtable flushes queued) and `interlaken_storage_l0_tables` (level-0 tables in the data keyspace) stand in for it, and `interlaken_storage_compaction_seconds` records time spent compacting.
- **Commit counts and latency are measured around the commit path,** in `Txn::commit`, so they cover the sequencer, the commit log and replicated commit paths alike. `interlaken_storage_commits_total` counts transactions with writes; read-only and empty commits don't reach a commit path and aren't counted. Group sizes come from the sequencer, so a replicated database doesn't record them yet.
- **Added:** `interlaken_storage_active_transactions` and `interlaken_storage_expired_transactions_total`, sampled alongside the oldest snapshot's age.
- **The sampler** is a thread started by `FjallStorage::open`, every `StorageOptions::metrics_interval` (default 10 seconds, `None` for none). `FjallStorage::record_metrics` records a sample on demand.
- **Declarations** live in `interlaken_storage::telemetry`: the `Metric` type, each crate's `METRICS` list, and `describe`, which passes declarations to the installed recorder. The book's metric reference (`operations/metrics.md`) is generated from them.
- **The commit span isn't linked to its group's span.** One group answers many commits; linking them needs the group span's ID carried back with each answer, which can come later if traces need it.
- **The test recorder and subscriber** are written against the `metrics` and `tracing` traits directly, without `metrics-util` or `tracing-subscriber`. Their values are process-wide, so a test binary that checks metrics serialises its tests.

### Phase 2: statements and `Usage`

- Count rows, bytes, CPU time and peak memory per statement from the executor's existing counters, and return `Usage` with results.
- Record the statement, memory and authorization metrics, and the statement spans.

#### As implemented (phase 2)

- **`Usage` fields:** `rows_out` became `rows_returned`, the rows a statement returned; rows a DML statement changes without `RETURNING` aren't counted separately. `bytes_written` is the key and value bytes the statement added to its transaction's write set (`Txn::write_set_bytes` before and after), which is what its commit writes; a failed or cancelled statement's is zero. `peak_memory` summed over a transaction is the largest statement's.
- **Where `Usage` appears:** `QueryResult::usage`, `QueryStream::usage()` once the stream has ended, and `Session::transaction_usage()`, the sum for the open or last transaction. DDL, policy and administrative methods don't return usage; they are counted in the statement metrics only.
- **Reads are counted for every statement** by `UsageCounters` in the executor's context: scans and lookups add each storage call's rows and bytes, separately from `EXPLAIN ANALYZE`'s per-operator counters. Rows of index entries count towards bytes but not rows scanned.
- **CPU time** is measured around each poll of the plan's root stream, which is also where the `interlaken.execute` span is entered. Decoding on storage threads isn't counted.
- **Statement kinds:** queries and DML are labelled `select`, `insert`, `update` or `delete` by the first writing operator in the plan, or `unplanned` if they fail before planning finishes. The other methods are labelled `ddl`, `policy`, `admin` or `other` (`prepare`, `explain`, `privileges`). Outcomes are `ok`, `error` and `cancelled` (a stream dropped before its end; a DML statement that then finishes in the background is counted up to that point).
- **Added:** `interlaken_statement_cpu_seconds` (a histogram of executor time per statement, whose sum is the total) and `interlaken_bytes_written_total`.
- **Denials** are counted by error: `column` (a column policy), `write` (a write check), `statement` (a missing privilege or another security error). Rows policies filter out aren't denials, and denials carry no table names.
- **Policy proposals** are counted by step (`submit`, `verify`, `activate`) and outcome (`ok`, `error`).
- **UDF time** isn't recorded: WASM UDFs (ADR-0029) aren't built.
- **`interlaken_engine::describe_metrics()`** describes every crate's metrics in the build, and `interlaken_engine::METRICS` lists the engine's.

### Phase 3: replication and the network

- Follow openraft's metrics watch into the Raft gauges.
- Record proposal latency, read paths, snapshot transfers, and transport bytes and calls per peer, in `interlaken-replication` and `interlaken-net`.
- Add the Raft spans.

#### As implemented (phase 3)

- **Raft's gauges carry a `node` label** (the replica's ID), so several replicas in one process, as in tests, don't overwrite each other's values. Follower lag also carries `peer`. A task per replica follows openraft's metrics watch and updates them on every change; it ends when Raft stops.
- **Added:** `interlaken_raft_proposal_size`, the transactions in each proposal: the replicated counterpart of the sequencer's group size, which a replicated database doesn't record.
- **Reads** count fresh-read requests by path: `lease` or `read_index` on the leader (which one depends on `ReplicaConfig`), `forwarded` from a follower. Reads at `Freshness::Local` don't involve Raft and aren't counted.
- **Proposal latency** runs from handing a group to Raft until it is committed and applied on the leader, under the `interlaken.raft.propose` span.
- **Snapshot transfers** are counted on both sides: sent by the leader's network (`interlaken.raft.snapshot.send`), received by the replica's handler, with bytes per chunk, and the duration from the first chunk to the end of the install (`interlaken.raft.snapshot.install`, which covers finishing the staged generation and openraft's install).
- **Transport metrics** are split by side. Calls a replica makes are counted per `peer`, by request kind and outcome, with request bytes `sent` and response bytes `received`. Requests it answers are counted in total (`interlaken_net_served_total`, `interlaken_net_served_bytes_total`), since the server doesn't know which peer sent them. Bytes are postcard-encoded messages, without gRPC and TLS framing, so actual network traffic is somewhat higher.
- **`describe_metrics`** includes replication's and the network's metrics when the engine is built with `cluster`; the book's reference lists them always.

### Phase 4: attribution and planner feedback

- Add the `Attribution` setting and the capped usage counters.
- Record the q-error of top-level estimates. This needs ADR-0036's estimates; until they exist, the planner's current row estimates are used.

#### As implemented (phase 4)

- **Settings:** `EngineOptions::attribution` (`Attribution::Off`, `Principal` or `Context(name)`) and `EngineOptions::attribution_limit` (default 1,000).
- **Off records nothing:** with attribution off, the `interlaken_usage_*` metrics aren't recorded at all, rather than recorded without a label. Their totals would duplicate `interlaken_rows_scanned_total`, `interlaken_bytes_read_total`, `interlaken_bytes_written_total` and the sum of `interlaken_statement_cpu_seconds`.
- **Keys are chosen per session,** when it starts: a session's principal and context are fixed. So the limit, and `interlaken_usage_attribution_overflow_total`, count sessions with new keys, not statements. `Context(name)` reads the application context field `name` (`context.app.name`); a session without it counts as `none`, and one whose value isn't a string or integer as `other`.
- **CPU per key** is `interlaken_usage_cpu_seconds`, a gauge that only rises, like the other time totals.
- **Usage is attributed by statement,** as each ends; a transaction's statements count towards its session's key.
- **Planner feedback is deferred.** The planner makes no row estimates yet: ADR-0036, which adds them, is still a proposal. Recording the q-error moves to ADR-0036's implementation, which has the estimates and can compare them with the actual rows `EXPLAIN ANALYZE` already counts per operator.

### Phase 5: export

- `install_prometheus` and `install_otlp` in `interlaken-telemetry`.
- The instrumentation-cost benchmarks, run in CI's nightly job.
- A book chapter on monitoring: the cost ratios from section 3, what each dashboard should show, and the privacy note on attribution.

## Consequences

### Positive

- Running cost becomes measurable: disk bytes, network bytes and CPU per committed byte or per statement, rather than one-off tests like ADR-0038's.
- Applications can bill, set quotas or find expensive workloads from `Usage`, with or without a metrics backend.
- The planner's estimates are checked in production, which ADR-0036 needs.
- Hosts choose where telemetry goes; Interlaken never decides for them.

### Negative

- Two new dependencies in every library crate, though both are small and widely used.
- Every metric name is a documented interface. Renaming one breaks dashboards, so names need care up front.
- Counting CPU per poll adds a clock read per poll of a statement's root stream. The benchmark gate exists to catch it if that costs more than expected.
- Attribution by principal puts identifiers in a separate system with its own access control. It is off by default, and documented.

## Open Questions

- **Should `Usage` include an estimate of replication cost,** for example the statement's write bytes times the number of replicas? It would be approximate, but it is what a tenant actually costs in a cluster.
- **Should fjall's internal counters come from fjall or from the file system?** fjall exposes some sizes but not bytes written. Sampling `/proc/self/io` counts all of the process's writes, logs included.
- **Should attribution be per statement or per transaction?** A transaction's commit writes for all its statements; per-statement attribution of `bytes_written` needs the write set split by statement.
- **Should the q-error be recorded for every operator** rather than the top-level ones, sampled to keep the cost down?

## Future Work

- Quotas and admission control driven by `Usage`: refuse or delay a workload that exceeds its budget.
- Live views (`interlaken_stat_statements`, `interlaken_stat_activity`) queried with SQL, like Postgres's `pg_stat_*`, governed by the same policies as other tables.
- Cost-aware snapshot and compaction scheduling, using the bytes-written and network figures to decide when to compact or which peer to send a snapshot from.

## References

- [ADR-0027: Query memory management](0027-query-memory-management.md)
- [ADR-0028: Physical operators, planning and EXPLAIN](0028-physical-operators-planning-and-explain.md)
- [ADR-0031: Engine sessions and the storage-backed catalog](0031-engine-sessions-and-storage-backed-catalog.md)
- [ADR-0033: Executor performance](0033-executor-performance.md)
- [ADR-0036: Statistics and cost-based planning](0036-statistics-and-cost-based-planning.md)
- [ADR-0037: Replication with Raft](0037-replication-with-raft.md)
- [ADR-0038: Streaming snapshots](0038-streaming-snapshots.md)
- `tracing`: <https://docs.rs/tracing>
- `metrics`: <https://docs.rs/metrics>
- Prometheus naming conventions: <https://prometheus.io/docs/practices/naming/>

[`CommitStats`]: ../../crates/storage/src/commit/stats.rs
[`GcStats`]: ../../crates/storage/src/gc.rs
[`tracing`]: https://docs.rs/tracing
[`metrics`]: https://docs.rs/metrics
