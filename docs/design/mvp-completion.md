# MVP Completion: Final Milestones

*Draft — 2026-11-20 · Companion to `mvp-fjall-storage-design.md`, `ir-design.md` and `executor-design.md`*

## Summary

The storage (M1–M6), IR (I1–I5) and executor (E1–E5) milestones are complete. Each layer is tested on its own and against the layers below it. Four gaps remain before the MVP is a usable embedded database:
- **No integration.** Nothing joins the layers. The IR catalog exists only in memory, and no API takes a statement and a principal and runs it through resolve, verify, secure, optimize, plan and execute in a transaction.
- **Authorization data can go stale.** Since E4, DML can change a membership edges table without updating its closure table, and some policies still fail closed where they should work.
- **Performance claims are unmeasured.** The executor's success criterion (10 million rows in bounded memory) has not been demonstrated. Known inefficiencies (row-at-a-time writes, per-row index probes, aggregates that keep every argument) have not been measured.
- **Durability is tested only against process crashes,** not power loss, and disk space is not shown to stay bounded.

These milestones close those gaps. They draw on the deferred ADR phases and open backlog items that belong to the MVP. The rest are listed under [Beyond the MVP](#beyond-the-mvp).

## Milestones

Roughly 9–10 weeks for one experienced engineer. F2 and F3 can run in parallel once F1 is done; F4 can run at any time.

| # | Milestone | Duration | Exit criteria |
| --- | --- | --- | --- |
| F1 | Engine integration: storage-backed catalog, all column types, the statement pipeline | 3–4 weeks | IR text and a principal in, rows out, through one public API. The security oracle passes through that API. The catalog survives a restart |
| F2 | Authorization completeness: closure maintenance in DML, entity loading, column masking, impact analysis at scale | 2–3 weeks | The security oracle passes with memberships changed by DML in the same transaction. Policies over non-principal entities and per-row masks work |
| F3 | Executor performance: incremental aggregates, batched index lookups and writes, cooperative scheduling, benchmarks | 2–3 weeks | A 10-million-row streaming query runs in bounded memory. Benchmarks record baselines. Default memory budgets are chosen from measurements |
| F4 | Durability hardening: power-loss testing, disk-space soak | 1–2 weeks | Every `Sync`-acknowledged commit survives injected power loss. Disk space levels off under a sustained update workload |

### F1: Engine integration

*Status: complete (2026-11-20), recorded in [ADR-0031](0031-engine-sessions-and-storage-backed-catalog.md).*

The `interlaken-engine` crate becomes the one entry point.
- **Storage-backed IR catalog** (ADR-0013 phase 3). An IR `Catalog` read from storage's `sys_*` tables, extended with what the IR needs and storage lacks:
  - IR types per column;
  - entity and membership declarations;
  - a catalog-wide schema version that resolved plans pin.

  DDL goes through the engine, so the storage and IR catalogs change in one transaction. Built-in functions stay in code. Catalog lookups read through the transaction, so plans and DDL conflict as ADR-0019 requires.
- **All IR scalar types as columns** (`storage-temporal-types`): `date`, `timestamptz`, `uuid` and `jsonb` in the tuple encoding, row format and catalog codes, with the encoding property tests extended. After this, `stored_type` covers every IR scalar type and arrays of them.
- **The statement pipeline.** A `Session` per connection holds a principal's `SecurityContext`. It runs statements in a `Transaction` (ADR-0026):
  1. resolve against the transaction's catalog;
  2. verify;
  3. load the principal (ADR-0017), from the residual cache where possible (ADR-0019);
  4. secure;
  5. optimize;
  6. plan physically and execute.

  It also covers `EXPLAIN [ANALYZE]` with policy visibility from the principal's privileges, parameters, and a `statement_timeout` that cancels the stream (`executor-design.md`, Timeouts).
- **Roles for privileged catalog changes** (`leakproof-admin-only`). Only administrators can mark functions leakproof or change policies. Only principals with policy privilege see policy logic in `EXPLAIN`.
- **Exact `EXPLAIN` redaction** (`explain-policy-markers`): security lowering marks the operators it adds. This lands before `EXPLAIN` reaches clients through the session API.
- *Testing:*
  - The security oracle and leak suite through the public API, over a catalog created by engine DDL.
  - A restart test: catalog, data and policies persist, and resolved plans pinned to an old schema version are rejected after DDL.
  - Concurrent DDL and queries conflict as ADR-0019 specifies.

An ADR records the session API and the storage-backed catalog's tables.

### F2: Authorization completeness

- **Closure maintenance in DML** (`closure-maintenance-in-dml`). The physical planner adds a closure-maintenance step to any DML on a membership edges table. It writes the path-counted closure rows (`security::Closure`, ADR-0017) in the same statement, so authorization in the same transaction sees the new memberships. This is the most important item here: E4 made edge DML possible, and today it leaves the closure stale.
- **Entities beyond the principal** (`entity-loading-beyond-principal`). Partial evaluation loads referenced entities, such as `principal.manager.dept`, on demand through the transaction, registering their reads like the principal's.
- **Column masking** (`column-masking`). Per-row column policies return masked values through masking functions instead of denying the column outright. `EXPLAIN` hides masks like residuals.
- **Impact analysis at scale** (`impact-analysis-at-scale`). Prune by the policies' resource types, evaluate incrementally per changed policy, and sample above a size threshold, reporting when results are sampled.
- *Testing:*
  - The security oracle gains DML on edges tables interleaved with queries in one transaction, against a model that recomputes closures from scratch.
  - Serializable conflicts between membership changes and authorization (as in `engine/tests/authorization.rs`) also cover DML-maintained closures.
  - Oracle cases cover entity paths beyond the principal, and differential cases cover masks.

### F3: Executor performance

- **Benchmarks first.** A `benches/` suite for the executor:
  - scans with pushdown;
  - hash joins;
  - aggregates;
  - sorts and top-N;
  - index nested loops;
  - bulk DML;
  - secured queries.

  It runs over 1 to 10 million rows and records throughput and peak memory. Each item below is kept only if it moves its benchmark.
- **The 10-million-row criterion** (`executor-design.md` success criteria, ADR-0027): a streaming query over 10 million rows holds memory flat. A blocking query over them fails cleanly at its budget.
- **Incremental aggregates** (`incremental-aggregates`): accumulators for `sum` over integers (in `i128`), `min`, `max`, `bool_and` and `bool_or`, so memory is proportional to the number of groups. Float `sum` and `avg` keep tuples unless an exact incremental form matches the interpreter.
- **Batched index lookups** (`batched-index-lookup`): an `IndexLookup` join that probes one batch of outer keys per storage call, replacing per-row applies for equi-joins on indexed keys.
- **Batched row writes** (`batched-row-writes`): a row-layer batch API for insert, update and delete, with constraint checks in one `get_many` per batch.
- **Set-oriented `NOT IN`** (`not-in-decorrelation`): a null-aware anti join, so `NOT IN` and `ALL` over large subqueries stop being nested loops.
- **Cooperative scheduling** (`executor-design.md`): hash builds, sorts and large aggregations yield to the Tokio scheduler every few batches.
- **Decode on the storage thread** (ADR-0023 phase 3), only if profiles show row decoding on executor threads.
- **Default memory budgets** (`default-memory-budgets`): choose the per-query default and a recommended node pool size from the benchmarks' peak-memory figures, and record them in ADR-0027.
- *Testing:*
  - The existing differential tests cover every changed operator; the generators gain large groups and index-keyed joins.
  - Planted-bug checks for each new operator path, as in E1–E5.

### F4: Durability hardening

- **Power-loss testing** (`lazyfs-power-loss`): the crash workload on LazyFS, dropping unsynced writes. Every commit acknowledged under `Sync` survives; `Buffered` commits may be lost but never corrupt the database.
- **Disk-space soak** (`disk-space-boundedness`): a long sustained-update run asserting that `StorageMetrics::disk_space_bytes` levels off once compaction catches up with GC.
- **Commit stalls on target hardware** (`fsync-stalls-other-hardware`): rerun when the deployment hardware is known; it doesn't block the milestone.
- *Testing:* both runs are scripted, so CI can run them nightly.

## Beyond the MVP

These are separate projects, or wait on one. They keep their backlog entries and ADR phases.

| Item | Waits on |
| --- | --- |
| SQL front end, with `subquery-positions` and `interp-text-formats`; sqllogictest and differential runs against Postgres (`executor-design.md`) | The SQL front end project, which starts on F1's session API |
| GraphQL front end (serializing `Nest` output, ADR-0012) | The GraphQL front end project |
| WASM UDFs: `UdfProject` and wasmtime (ADR-0029 phase 2), the WIT interface (`wasm-wit-interface`) | The WASM runtime project |
| Replication: Raft in place of the sequencer, provers watching replicated proposals (ADR-0030 phase 2, `prover-subscriptions`), shared residual caches (`ir-design.md` open questions) | The Raft project |
| `pipelined-commit-validation` | A measured commit bottleneck at 4–32 clients |
| Stream aggregation, merge join, spilling, cost-based planning (ADR-0028 future work) | Statistics and real workloads |

## Risks

- **Catalog migration.** F1 changes how the catalog is stored, and on-disk format stability is out of scope for the MVP (`mvp-fjall-storage-design.md`). Databases created before F1 are not migrated.
- **Closure maintenance cost.** Path-counted maintenance on deep hierarchies can write many closure rows per edge change. F2 measures this on the benchmark fixtures, and caps or batches it if needed.
- **Benchmark scope creep.** F3 optimizes only what the benchmarks show; anything else returns to the backlog.
