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

### lazyfs-power-loss: Power-loss testing with a fault-injecting filesystem
- **Origin:** ADR-0009 phase 6, ADR-0005 (crash harness section), M6
- **Why:** The crash harness kills the process with `SIGKILL`. That loses nothing already handed to the kernel, so it checks crash consistency and recovery, not power loss. A bug in how `Durability::Sync` reaches stable storage (fjall's `SyncAll`, directory fsyncs, journal rotation) would go unnoticed. Run the crash workload on LazyFS (or a similar FUSE filesystem that drops unsynced writes on demand) and check that every commit acknowledged under `Sync` survives while `Buffered` commits may be lost.
- **When:** Before anything relies on `Sync` durability in production.
- **Size:** About 2–3 days. Most of the time goes on LazyFS setup and CI plumbing; the checks in `tests/crash.rs` can be reused.
- **Status:** open

### fsync-stalls-other-hardware: Re-measure commit stalls on other hardware
- **Origin:** ADR-0005 measurements, M3
- **Why:** Under `Sync`, rare commit stalls of 0.2–2 s matched raw fsync stalls on the development disk (up to 354 ms with plain `fsync`). We concluded they come from the disk, not the sequencer, but checked on only one machine. Run `benches/commit_throughput.rs` and a raw-fsync probe on the target deployment storage (cloud block volume, NVMe) and record p99/max commit latency in ADR-0005.
- **When:** Once the target deployment hardware is known, or if latency SLOs are set.
- **Size:** About half a day.
- **Status:** open

### disk-space-boundedness: Assert disk space, not only versions, stays bounded
- **Origin:** ADR-0009 phase 5, M5/M6
- **Why:** `space_stays_bounded_under_sustained_updates` shows the number of live versions stays bounded, but not on-disk bytes. GC removes are physical fjall deletes, so space comes back only after compaction drops the tombstones. A long soak should track `StorageMetrics::disk_space_bytes` and assert it levels off under a steady update workload.
- **When:** Before the first long-running deployment, or sooner if disk growth shows up.
- **Size:** About 1 day, plus a soak run.
- **Status:** open

### pipelined-commit-validation: Validate the next commit group while the current one syncs
- **Origin:** ADR-0005 measurements, M3
- **Why:** Under `Sync`, the sequencer is idle during each group's fsync (about 230–970 µs). Validating the next group meanwhile would raise throughput at moderate concurrency. Not needed at MVP scale.
- **When:** Only if commit throughput or latency at 4–32 clients becomes a bottleneck.
- **Size:** About 3–5 days, including model and crash-test updates.
- **Status:** open (low priority)

## IR

### leakproof-admin-only: Enforce that only administrators mark functions leakproof
- **Origin:** ADR-0011, I2
- **Why:** Leakproofness decides whether a predicate may move below a security barrier. The in-memory catalog accepts any `FunctionDef`. The rule belongs with catalog DDL and roles, which do not exist yet.
- **When:** With the storage-backed catalog (ADR-0013 phase 3), or when user-defined functions arrive, whichever is first.
- **Size:** About 1 day once roles exist.
- **Status:** open

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

### not-in-decorrelation: Decorrelate NOT IN, ALL and general ANY
- **Origin:** ADR-0012 (decorrelation), I3
- **Why:** These lowerings aggregate over a comparison with the outer row, so they stay as nested-loop applies. A null-aware anti join, or a rewrite that pre-aggregates the subquery's NULL count, would make `NOT IN` against a large subquery set-oriented.
- **When:** When a workload shows `NOT IN` or `ALL` over large subqueries, or with the executor's join operators (E3).
- **Size:** About 3–5 days, including differential test coverage.
- **Status:** open

### interp-text-formats: Widen the reference interpreter's text input formats
- **Origin:** ADR-0021 phase 3, I3
- **Why:** Casts from text accept only ISO dates and timestamps, hex `bytea` and a basic set of booleans, and `jsonb` is stored unvalidated. Postgres accepts more. The executor's kernels will be tested against the interpreter, so the two must agree.
- **When:** Executor milestone E2 (expression kernels).
- **Size:** About 2 days.
- **Status:** open

## Executor and IR open questions

### default-memory-budgets: Choose default per-query and per-node memory budgets
- **Origin:** ADR-0027 open questions, `executor-design.md`
- **Why:** Queries that exceed the budget fail rather than spill, so the defaults decide which real queries succeed. They need numbers for the container sizes we expect.
- **When:** Before executor milestone E5, ideally after measuring E3 operators on realistic data.
- **Size:** About 1 day of measurement, plus an ADR update.
- **Status:** open

### hash-join-build-side: Choose the hash join build side at planning time or at run time
- **Origin:** `executor-design.md` open questions
- **Why:** Choosing at run time, after seeing input sizes, avoids bad plans when estimates are poor, but complicates the operator.
- **When:** During executor milestone E3.
- **Size:** A decision during E3.
- **Status:** open

### wasm-wit-interface: Define the WIT interface for WASM UDFs
- **Origin:** ADR-0029 open questions
- **Why:** Guests need an interface for Arrow batches and host calls. The executor's batch format must not rule it out, but the interface itself is deferred.
- **When:** When WASM UDF work is scheduled (after the executor MVP).
- **Size:** About 1 week of design.
- **Status:** open

## Done

### nest-output-format: Decide `Nest` output: jsonb or Arrow list/struct
- **Origin:** `executor-design.md` open questions, ADR-0012
- **Why:** jsonb is simpler. Arrow list/struct columns keep types and are cheaper for the GraphQL serializer. The choice shapes the `Nest` operator and the serializer interface.
- **When:** Before executor milestone E5.
- **Size:** A decision, plus an ADR update.
- **Status:** dropped: already decided in ADR-0012 (typed `array(record(...))`, Arrow list-of-struct at run time, JSON only at serialization). The open question in `executor-design.md` predates that ADR.
