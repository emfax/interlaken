# Backlog

Follow-up work found during implementation, plus open questions from the ADRs that need an answer before a known point.

- **ADRs keep the decisions; this file keeps the work.** Each entry links to the ADR it came from. Long-range "Future Work" items (Raft, spilling, JIT and so on) stay in their ADRs until someone schedules them.
- **Every entry has a "When":** the milestone or event that makes it matter. Review the backlog at the start of each milestone and pull in whatever that milestone triggers.
- **For a `TODO` in code,** point at the entry's anchor, e.g. `// TODO(backlog: lazyfs-power-loss)`.
- **When an item is done,** set its status to `done (<commit>)` and move it to [Done](#done). If it is dropped, say why.

Entry format:

```markdown
### <anchor-slug>: <title>
- **Origin:** where it came from (ADR, milestone, review)
- **Why:** the gap or risk
- **When:** the trigger that makes it matter
- **Size:** rough estimate
- **Status:** open | in progress | done (<commit>) | dropped (<reason>)
```

## Storage

### fsync-stalls-other-hardware: Re-measure commit stalls on other hardware
- **Origin:** ADR-0005 measurements, M3
- **Why:** Under `Sync`, rare commit stalls of 0.2–2 s matched raw fsync stalls on the development disk (up to 354 ms with plain `fsync`). We concluded they come from the disk, not the sequencer, but checked on only one machine. Run `scripts/commit-stalls.sh DIR` with `DIR` on the target deployment storage (cloud block volume, NVMe). It runs a raw fsync probe and the `Sync` rows of `benches/commit_throughput.rs`, which report p50, p99 and max commit latency. Record the figures in ADR-0005.
- **When:** Milestone F4 (`design/mvp-completion.md`), but it doesn't block the milestone: once the target deployment hardware is known, or if latency SLOs are set.
- **Size:** About an hour now that the script exists.
- **Status:** open. F4 added the script and a short baseline on the development disk (ADR-0005, *As implemented (F4)*); the deployment hardware is still unknown.

### nightly-durability-ci: Run the power-loss and disk-space scripts nightly
- **Origin:** F4
- **Why:** `scripts/power-loss.sh` (LazyFS, 50 cycles per test) and `scripts/disk-space-soak.sh` (10 minutes) are too slow for every commit. The normal suite runs only short versions, and skips power loss without `LAZYFS_ROOT`. Both scripts exit non-zero on failure, but nothing runs them yet. The CI runner needs FUSE (`/dev/fuse`, `fusermount3`), CMake and g++. It also needs either `libfuse3-dev` or `apt-get download`; `scripts/build-lazyfs.sh` handles the latter.
- **When:** When the repository gets CI.
- **Size:** About half a day.
- **Status:** open

### pipelined-commit-validation: Validate the next commit group while the current one syncs
- **Origin:** ADR-0005 measurements, M3
- **Why:** Under `Sync`, the sequencer is idle during each group's fsync (about 230–970 µs). Validating the next group meanwhile would raise throughput at moderate concurrency. Not needed at MVP scale.
- **When:** Only if commit throughput or latency at 4–32 clients becomes a bottleneck.
- **Size:** About 3–5 days, including model and crash-test updates.
- **Status:** open (low priority)

## IR

### subquery-positions: Lower subqueries in the positions the resolver rejects
- **Origin:** ADR-0013 (resolver), I2
- **Why:** The resolver rejects subqueries:
  - in `CASE` and `COALESCE` branches, where eager lowering would evaluate them, and possibly raise errors, for rows whose branch is not taken;
  - in outer, semi and anti join conditions;
  - in `VALUES`, `LIMIT`/`OFFSET` and `RETURNING`.
  Postgres accepts all of these. Conditional branches need a lazy form of `Apply` or a guarded lowering.
- **When:** When the SQL front end meets them in real queries.
- **Size:** About 2–4 days, depending on the conditional-branch design.
- **Status:** open

### closure-maintenance-in-dml: Maintain closure tables in the same transaction as edge changes
- **Origin:** ADR-0017, I4
- **Why:** `security::Closure` implements path-counted maintenance, and principal loading reads the closure tables. Nothing yet writes closure rows when an edges table changes. Until something does, closure tables must be maintained by whoever changes memberships.
- **When:** Milestone F2 (`design/mvp-completion.md`). Originally: executor milestone E4 (DML sinks), which is the natural hook. Required before memberships change through SQL.
- **Size:** About 3 days, including tests that interleave edge changes with authorization.
- **Status:** open

### column-masking: Per-row column access and masking functions
- **Origin:** ADR-0018, I4
- **Why:** A column policy that depends on the row denies the column outright (fail closed). Returning redacted values per row needs masking functions.
- **When:** Milestone F2 (`design/mvp-completion.md`). Originally: when an application needs per-row column visibility.
- **Size:** About 1 week.
- **Status:** open

### entity-loading-beyond-principal: Load entities other than the principal for partial evaluation
- **Origin:** ADR-0017, I4
- **Why:** `load_principal` loads only the principal's attributes and ancestors. A policy reading another entity through the principal (`principal.manager.dept`) fails, and the policy is then ignored.
- **When:** Milestone F2 (`design/mvp-completion.md`). Originally: when policies need it. Load referenced entities on demand through the transaction.
- **Size:** About 1–2 days.
- **Status:** open

### impact-analysis-at-scale: Make impact analysis scale with the data
- **Origin:** ADR-0030, I5
- **Why:** Impact analysis authorizes every principal against every governed row and column. That is fine for small and medium data, but quadratic.
- **When:** Milestone F2 (`design/mvp-completion.md`). Originally: when a policy change takes noticeably long to verify.
- **Size:** About 3–5 days, for sampling, pruning by the policies' resource types, and incremental evaluation.
- **Status:** open

### prover-subscriptions: Provers that watch replicated proposals
- **Origin:** ADR-0030 phase 2
- **Why:** `prove_pending` must be called. With replication, prover nodes should run it whenever a proposal arrives.
- **When:** With Raft.
- **Size:** About 1–2 days once change notifications exist.
- **Status:** open

### not-in-decorrelation: Decorrelate NOT IN, ALL and general ANY
- **Origin:** ADR-0012 (decorrelation), I3
- **Why:** These lowerings aggregate over a comparison with the outer row, so they stay as nested-loop applies. A null-aware anti join, or a rewrite that pre-aggregates the subquery's NULL count, would make `NOT IN` against a large subquery set-oriented.
- **When:** Milestone F3 (`design/mvp-completion.md`). Originally: when a workload shows `NOT IN` or `ALL` over large subqueries, or with the executor's join operators (E3).
- **Size:** About 3–5 days, including differential test coverage.
- **Status:** open

### interp-text-formats: Widen the reference interpreter's text input formats
- **Origin:** ADR-0021 phase 3, I3
- **Why:** Casts from text accept only ISO dates and timestamps, hex `bytea` and a basic set of booleans, and `jsonb` is stored unvalidated. Postgres accepts more. Since E2 the executor's cast kernels call the interpreter's `cast_value` for text, so widening it widens both at once.
- **When:** With the SQL front end, when client-supplied text literals arrive in other formats.
- **Size:** About 2 days.
- **Status:** open

## Executor

### batched-index-lookup: Probe an index with a whole input batch in joins
- **Origin:** ADR-0023 phase 2, E4
- **Why:** An index nested loop currently runs through `Apply`, one index scan per outer row. An `IndexLookup` join operator that collects a batch's keys and calls `get_many` (or one ranged scan) per batch would cut storage calls by the batch size.
- **When:** Milestone F3 (`design/mvp-completion.md`). Originally: when joins on indexed keys show up in profiles, or with cost-based planning.
- **Size:** About 3–4 days.
- **Status:** open

### batched-row-writes: Write DML rows in batches
- **Origin:** E4
- **Why:** DML calls the row layer once per row, and each call makes its own constraint-check reads. A batch API in the row layer could probe all keys of a batch in one `get_many` and buffer its writes together.
- **When:** Milestone F3 (`design/mvp-completion.md`). Originally: when bulk inserts or large updates matter.
- **Size:** About 3 days, mostly in the storage row layer.
- **Status:** open

### incremental-aggregates: Accumulate sum, min, max and bool aggregates incrementally
- **Origin:** E3 (ADR-0028 phase 1)
- **Why:** Aggregates other than plain `count` keep every argument tuple until the group finishes, so memory grows with the input. Exact incremental accumulators (integer sums in `i128`, min and max) would make it proportional to the number of groups; float sums need care to stay identical to the interpreter.
- **When:** Milestone F3 (`design/mvp-completion.md`). Originally: when large groups show up. Since E5, kept argument tuples count against the query's memory budget, so large groups fail cleanly rather than exhaust the node.
- **Size:** About 2 days.
- **Status:** open

## Executor and IR open questions

### default-memory-budgets: Choose default per-query and per-node memory budgets
- **Origin:** ADR-0027 open questions, `executor-design.md`
- **Why:** Queries that exceed the budget fail rather than spill, so the defaults decide which real queries succeed. They need numbers for the container sizes we expect.
- **When:** Milestone F3 (`design/mvp-completion.md`). Originally: before production deployment, after measuring operators on realistic data. E5 kept the 256 MiB per-query default and configures no node pool unless asked (ADR-0027).
- **Size:** About 1 day of measurement, plus an ADR update.
- **Status:** open

### wasm-wit-interface: Define the WIT interface for WASM UDFs
- **Origin:** ADR-0029 open questions
- **Why:** Guests need an interface for Arrow batches and host calls. The executor's batch format must not rule it out, but the interface itself is deferred.
- **When:** When WASM UDF work is scheduled (after the executor MVP).
- **Size:** About 1 week of design.
- **Status:** open

## Done

### lazyfs-power-loss: Power-loss testing with a fault-injecting filesystem
- **Origin:** ADR-0009 phase 6, ADR-0005 (crash harness section), M6
- **Why:** The crash harness kills the process with `SIGKILL`. That loses nothing already handed to the kernel, so it checks crash consistency and recovery, not power loss. A bug in how `Durability::Sync` reaches stable storage (fjall's `SyncAll`, directory fsyncs, journal rotation) would go unnoticed. Run the crash workload on LazyFS (or a similar FUSE filesystem that drops unsynced writes on demand) and check that every commit acknowledged under `Sync` survives while `Buffered` commits may be lost.
- **When:** Milestone F4 (`design/mvp-completion.md`). Originally: before anything relies on `Sync` durability in production.
- **Size:** About 2–3 days. Most of the time goes on LazyFS setup and CI plumbing; the checks in `tests/crash.rs` can be reused.
- **Status:** done (F4): `tests/crash.rs` runs on LazyFS (`LAZYFS_ROOT`, `scripts/power-loss.sh`). No `Sync` commit was lost; `Buffered` loses only a suffix. It found and fixed `Buffered` losing commits on a process crash (ADR-0005, ADR-0009).

### disk-space-boundedness: Assert disk space, not only versions, stays bounded
- **Origin:** ADR-0009 phase 5, M5/M6
- **Why:** `space_stays_bounded_under_sustained_updates` shows the number of live versions stays bounded, but not on-disk bytes. GC removes are physical fjall deletes, so space comes back only after compaction drops the tombstones. A long soak should track `StorageMetrics::disk_space_bytes` and assert it levels off under a steady update workload.
- **When:** Milestone F4 (`design/mvp-completion.md`). Originally: before the first long-running deployment, or sooner if disk growth shows up.
- **Size:** About 1 day, plus a soak run.
- **Status:** done (F4): `tests/disk_space.rs` (12 s in the suite, `scripts/disk-space-soak.sh` for long runs) shows disk space levelling off, bounded by fjall's journal cap. The cap is now configurable through `StorageOptions::max_journal_bytes` and `memtable_bytes` (ADR-0008).

### storage-temporal-types: Store dates, timestamps and UUIDs
- **Origin:** E5
- **Why:** Storage has types only for the scalars the MVP tables needed (plus, since E5, arrays of them). The IR and executor support `date`, `timestamptz`, `uuid` and `jsonb`, but tables can't hold them, so `stored_type` returns `None` for them.
- **When:** Milestone F1 (`design/mvp-completion.md`). Originally: when the SQL front end needs those column types.
- **Size:** About 2–3 days (tuple encoding, row format, catalog codes, property tests).
- **Status:** done (F1): `date`, `timestamptz`, `uuid` and `jsonb` columns and arrays of them, in the tuple encoding, row format and catalog codes; `jsonb` isn't keyable (ADR-0003, ADR-0031).

### leakproof-admin-only: Enforce that only administrators mark functions leakproof
- **Origin:** ADR-0011, I2
- **Why:** Leakproofness decides whether a predicate may move below a security barrier. The in-memory catalog accepts any `FunctionDef`. The rule belongs with catalog DDL and roles, which do not exist yet.
- **When:** Milestone F1 (`design/mvp-completion.md`). Originally: with the storage-backed catalog (ADR-0013 phase 3), or when user-defined functions arrive, whichever is first.
- **Size:** About 1 day once roles exist.
- **Status:** done (F1): only principals with the `admin` privilege can call `Session::set_function_leakproof`; overrides live in `engine_functions` (ADR-0011, ADR-0031).

### explain-policy-markers: Mark the subplans security lowering adds
- **Origin:** ADR-0025 phase 1, E5
- **Why:** `EXPLAIN` hides policy logic from principals without policy privilege, and it finds the parts to hide by plan shape: the inner side of joins and applies on the path from a policy filter down to its rows. That is safe for the plans lowering produces today, but an optimizer change that reshapes them could expose a policy subplan. A marker that lowering sets on the operators it adds would make the redaction exact.
- **When:** Milestone F1 (`design/mvp-completion.md`). Originally: before the optimizer gains rules that move joins across policy filters, or before `EXPLAIN` is exposed to untrusted clients.
- **Size:** About 1–2 days across the IR and the executor.
- **Status:** done (F1): lowering marks its subplans with the reserved `sec$` prefix, which the verifier rejects in client plans, and `EXPLAIN` hides marked subplans wherever they sit (ADR-0025, ADR-0031).

### nest-output-format: Decide `Nest` output: jsonb or Arrow list/struct
- **Origin:** `executor-design.md` open questions, ADR-0012
- **Why:** jsonb is simpler. Arrow list/struct columns keep types and are cheaper for the GraphQL serializer. The choice shapes the `Nest` operator and the serializer interface.
- **When:** Before executor milestone E5.
- **Size:** A decision, plus an ADR update.
- **Status:** dropped: already decided in ADR-0012 (typed `array(record(...))`, Arrow list-of-struct at run time, JSON only at serialization). The open question in `executor-design.md` predates that ADR.

### hash-join-build-side: Choose the hash join build side at planning time or at run time
- **Origin:** `executor-design.md` open questions
- **Why:** Choosing at run time, after seeing input sizes, avoids bad plans when estimates are poor, but complicates the operator.
- **When:** During executor milestone E3.
- **Size:** A decision during E3.
- **Status:** done (E3): chosen at plan time, always the right input, read completely before the left side probes it. Revisit with statistics (ADR-0028 future work).
