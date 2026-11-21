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
- **Status:** in progress. `.github/workflows/nightly.yml` runs both scripts, the full suite, the SMT prover tests and the slow ignored tests at 03:00 UTC when `main` has commits from the last 25 hours. It hasn't run yet: the repository has no GitHub remote. Mark this done after the first green run. Elle isn't run yet; it needs Java and the `elle-cli` jar on the runner.

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

### prover-subscriptions: Provers that watch replicated proposals
- **Origin:** ADR-0030 phase 2
- **Why:** `prove_pending` must be called. With replication, prover nodes should run it whenever a proposal arrives.
- **When:** With Raft.
- **Size:** About 1–2 days once change notifications exist.
- **Status:** open

### interp-text-formats: Widen the reference interpreter's text input formats
- **Origin:** ADR-0021 phase 3, I3
- **Why:** Casts from text accept only ISO dates and timestamps, hex `bytea` and a basic set of booleans, and `jsonb` is stored unvalidated. Postgres accepts more. Since E2 the executor's cast kernels call the interpreter's `cast_value` for text, so widening it widens both at once.
- **When:** With the SQL front end, when client-supplied text literals arrive in other formats.
- **Size:** About 2 days.
- **Status:** open

## Executor

### batch-decoder: Decode rows into Arrow on the storage thread
- **Origin:** ADR-0023 phase 3, F3
- **Why:** Scans decode stored rows into `Datum`s and then Arrow on executor threads. F3's profile of a million-row scan put it at about 6% of CPU samples against 89% on storage pool threads, and pushed filters already keep rejected rows in the storage pool, so moving decode onto the storage thread would lengthen the calls scans wait on. Decoding straight from the row format into Arrow builders (skipping `Datum`) is the cheaper first step if it starts to matter.
- **When:** When profiles show executor threads saturated by decode, for example many concurrent large scans.
- **Size:** About 1 week for `BatchDecoder` in the storage trait; about 2 days for direct decode into builders.
- **Status:** open

### sort-yields: Let a large in-memory sort yield while sorting
- **Origin:** F3 (cooperative scheduling)
- **Why:** Blocking operators yield between input batches, but the final sort of a large buffered input is one call; at a few million rows it holds its thread for a second or more.
- **When:** When large sorts share threads with latency-sensitive queries.
- **Size:** About 2 days: sort runs a chunk at a time and merge them, yielding between chunks.
- **Status:** open

### undeclared-principal-types: Let principals of undeclared types be authorized
- **Origin:** writing the book's security chapters
- **Why:** Once a table backs an entity type, every principal's access to it goes through the policies. A principal whose type isn't a declared entity type, such as the `Admin::"root"` principal a database is typically created with, can't be named by Cedar policies, because the generated Cedar schema lists only declared entity types. While the default empty Cedar policy set is active, its queries on governed tables fail with `42501` "secured plan is invalid: Cedar partial request: principal type `Admin` is not valid for `Action::\"read\"`" rather than being plainly denied. So the first administrator can't load data into a newly governed table until an RLS policy set permits `(is principal Admin)`. Options:
  - add principal-only entity types (with no table) to the generated Cedar schema;
  - decide requests from undeclared principal types as denied rather than invalid;
  - require `Engine::create`'s administrator to be of a declared type.
- **When:** Before applications rely on Cedar policies with an administrator principal, or before the SQL front end, where roles are principals.
- **Size:** About 1–2 days, mostly in the Cedar schema and provider.
- **Status:** open

## Executor and IR open questions

### wasm-wit-interface: Define the WIT interface for WASM UDFs
- **Origin:** ADR-0029 open questions
- **Why:** Guests need an interface for Arrow batches and host calls. The executor's batch format must not rule it out, but the interface itself is deferred.
- **When:** When WASM UDF work is scheduled (after the executor MVP).
- **Size:** About 1 week of design.
- **Status:** open

## Done

### array-element-nullability: Convert between array element nullabilities
- **Origin:** writing the book's types chapter
- **Why:** An array type records whether its elements may be NULL, and array types convert only to themselves. So `(array "a" "b")`, of type `(array (not-null text))`, can't be inserted into an `(array text)` column (`42804`), and an explicit cast doesn't help. Converting `(array (not-null T))` to `(array T)` is lossless and should be implicit; the reverse could be an assignment conversion checked at run time.
- **When:** With the SQL front end, where `ARRAY['a', 'b']` into a `text[]` column is ordinary; or sooner if applications store arrays.
- **Size:** About 1 day in `typing` (implicit and assignment rules), the cast kernels and the interpreter.
- **Status:** done (ADR-0034 phase 1: `(array T)` converts implicitly to `(array (option T))`, and records likewise; the reverse isn't allowed)

### last-admin-guard: Refuse to revoke the last administrator
- **Origin:** writing the book's privileges chapter
- **Why:** `Session::revoke` lets an administrator revoke `admin` from every principal, themselves included. Nobody could then run DDL, change policies or grant privileges, and there is no recovery short of editing storage.
- **When:** Before production use.
- **Size:** A few hours: count remaining admins in the revoking transaction (registering the read so a concurrent revoke conflicts) and fail with `42501` or `55000`.
- **Status:** done (revoking `admin` counts the remaining grants and fails with `55000` if none remain; it rewrites each remaining `admin` grant so concurrent revocations conflict on a key at every isolation level, not only under SERIALIZABLE)

### parameter-validation: Check parameter values against their declarations
- **Origin:** writing the book's parameters chapter
- **Why:** `(param N TYPE)` declares a type, but the engine doesn't check the values passed against it before running the statement. A value of the wrong `Datum` variant fails at run time with `0A000` ("not supported by the executor yet: value Text(...) in a column of type int8"). A missing value fails with `0A000` ("not supported by the reference interpreter: parameter 1 has no value"). `Datum::Null` for a `(not-null T)` parameter is accepted, and the NULL then flows through a plan whose types say it can't occur. The optimizer and executor may rely on that, for example by simplifying `is-null` away. Postgres rejects such binds up front (`08P01`, `22P02`).
- **When:** Before the SQL front end's wire protocol, which binds client-supplied values.
- **Size:** About 1 day: check count, variant and nullability in the session before execution, with clear errors (`08P01` for the count, `42804` or `22P02` for the type, `22004` for NULL), and tests.
- **Status:** done (the session checks values against the parameters the resolved plan declares before running it: the count (`08P01`), each value's `Datum` variant recursively (`42804`) and NULL for NOT NULL parameters (`22004`), for text, prepared plans, streams and `EXPLAIN ANALYZE`; `interlaken_ir::interp::check_params`)

### engine-streaming-results: Stream query results through the session
- **Origin:** F3 (10-million-row criterion)
- **Why:** The executor streams, but `Session::query` collects every batch into a `QueryResult`, so a client reading 10 million rows through the engine holds them all. The 10-million-row test runs at the executor level for that reason.
- **When:** With the SQL front end's wire protocol, which streams rows to clients.
- **Size:** About 2 days: a result stream that keeps the statement's transaction and timeout.
- **Status:** done (`Session::query_stream` returns a `QueryStream` that borrows the session and pulls batches lazily under the statement's savepoint and timeout, committing autocommit at its end and undoing the statement on error; a dropped query is undone and a dropped DML statement runs to its end in the background; 3 million rows stream in under 2 MB of heap growth, ADR-0031 phase 2)

### masked-table-writes: Let principals write tables whose masked columns they don't read
- **Origin:** ADR-0032, F2
- **Why:** A statement that writes a table with a masked column fails for that principal (`42501`), as for a denied column, because DML carries the whole target row and check-columns counts that as reading every column. Masks are only unsafe where a write reads or returns the hidden value, or writes a masked value back.
- **When:** When an application masks columns of tables its users also write.
- **Size:** About 2 days: tell carried target-row columns from read ones in `check_columns`, and keep the target row unmasked.
- **Status:** done (writes read masked columns through their masks, carried target-row columns no longer count as read, and an unmasked read joined by primary key carries the row written; `RETURNING` is masked per written row, assignments see masked values, ADR-0032 phase 2)

### slice-memory-accounting: Count sliced batches by their own rows
- **Origin:** F3
- **Why:** Sort and aggregation outputs are split into 1,024-row slices of one buffer, and `RecordBatch::get_array_memory_size` reports the whole buffer for each slice. An operator that buffers such slices (a sort over a sort, a hash build over a sort) overcounts by up to the number of slices and can fail its budget far too early.
- **When:** When plans buffer the output of another blocking operator in practice, or before spilling.
- **Size:** About 1 day: account sliced bytes, or compact slices when buffering.
- **Status:** done (sorts and aggregations now emit compact batches gathered per pull, and `batch_bytes` counts a slice of a larger buffer by its own rows, following list offsets; a sort over a sliced `Nest` output fits 64 MiB where it reserved 386 MiB, ADR-0027 phase 3)

### memory-estimates: Make memory reservations match the heap
- **Origin:** F3 benchmarks
- **Why:** Hash joins, aggregations and `Distinct` estimate entries from `Datum` sizes plus a word or two; the benchmarks show resident memory up to 2.6 times the reservation (a million-row hash build reserved 132 MiB and grew the heap by 340 MiB). A finishing sort holds its buffered batches, their concatenation and the sorted copy at once but reserves only one of them (62 MiB reserved, up to 240 MiB resident). Budgets therefore cap less than they say, and the node pool recommendation has to allow for it.
- **When:** Before production deployment with a tight container limit.
- **Size:** About 2 days, measuring with a counting allocator in tests.
- **Status:** done (estimates count capacities, allocator chunks and hash-table buckets, sorts gather instead of concatenating and copying, keys are held once; every blocking operator reserves within 7% of its measured heap, `tests/memory_estimates.rs`, ADR-0027 phase 3)

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

### closure-maintenance-in-dml: Maintain closure tables in the same transaction as edge changes
- **Origin:** ADR-0017, I4
- **Why:** `security::Closure` implements path-counted maintenance, and principal loading reads the closure tables. Nothing yet writes closure rows when an edges table changes. Until something does, closure tables must be maintained by whoever changes memberships.
- **When:** Milestone F2 (`design/mvp-completion.md`). Originally: executor milestone E4 (DML sinks), which is the natural hook. Required before memberships change through SQL.
- **Size:** About 3 days, including tests that interleave edge changes with authorization.
- **Status:** done (F2): DML on an edges table maintains every closure the edge connects, path-counted, in the same statement; cycles fail with `23514`; concurrent changes conflict at every isolation level (ADR-0032).

### column-masking: Per-row column access and masking functions
- **Origin:** ADR-0018, I4
- **Why:** A column policy that depends on the row denies the column outright (fail closed). Returning redacted values per row needs masking functions.
- **When:** Milestone F2 (`design/mvp-completion.md`). Originally: when an application needs per-row column visibility.
- **Size:** About 1 week.
- **Status:** done (F2): column policies declare `:mask null` or a masking function (`redact`, `mask_last4`, any catalog function); row-dependent columns show masked values, hidden in `EXPLAIN` (ADR-0032).

### entity-loading-beyond-principal: Load entities other than the principal for partial evaluation
- **Origin:** ADR-0017, I4
- **Why:** `load_principal` loads only the principal's attributes and ancestors. A policy reading another entity through the principal (`principal.manager.dept`) fails, and the policy is then ignored.
- **When:** Milestone F2 (`design/mvp-completion.md`). Originally: when policies need it. Load referenced entities on demand through the transaction.
- **Size:** About 1–2 days.
- **Status:** done (F2): providers list the entity paths their policies read, and `load_entities` loads them through the transaction, registering reads like the principal's (ADR-0032).

### impact-analysis-at-scale: Make impact analysis scale with the data
- **Origin:** ADR-0030, I5
- **Why:** Impact analysis authorizes every principal against every governed row and column. That is fine for small and medium data, but quadratic.
- **When:** Milestone F2 (`design/mvp-completion.md`). Originally: when a policy change takes noticeably long to verify.
- **Size:** About 3–5 days, for sampling, pruning by the policies' resource types, and incremental evaluation.
- **Status:** done (F2): pruning by the changed policies' actions and resource types, incremental evaluation per changed policy, and seeded sampling above `ImpactOptions::max_decisions`, flagged in the report (ADR-0032).

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

### not-in-decorrelation: Decorrelate NOT IN, ALL and general ANY
- **Origin:** ADR-0012 (decorrelation), I3
- **Why:** These lowerings aggregate over a comparison with the outer row, so they stay as nested-loop applies. A null-aware anti join, or a rewrite that pre-aggregates the subquery's NULL count, would make `NOT IN` against a large subquery set-oriented.
- **When:** Milestone F3 (`design/mvp-completion.md`). Originally: when a workload shows `NOT IN` or `ALL` over large subqueries, or with the executor's join operators (E3).
- **Size:** About 3–5 days, including differential test coverage.
- **Status:** done (F3): `IN`, `NOT IN`, `op ANY` and `op ALL` decorrelate into left joins with aggregates of the subquery that do not read the outer row and, for `=`, its distinct non-NULL values, so `NOT IN` in a filter is a null-aware anti join; 1,000 rows `NOT IN` a 10,000-row subquery went from 11.2 s to 25 ms (ADR-0033).

### batched-index-lookup: Probe an index with a whole input batch in joins
- **Origin:** ADR-0023 phase 2, E4
- **Why:** An index nested loop currently runs through `Apply`, one index scan per outer row. An `IndexLookup` join operator that collects a batch's keys and calls `get_many` (or one ranged scan) per batch would cut storage calls by the batch size.
- **When:** Milestone F3 (`design/mvp-completion.md`). Originally: when joins on indexed keys show up in profiles, or with cost-based planning.
- **Size:** About 3–4 days.
- **Status:** done (F3): `IndexLookup` joins fetch the right rows for a whole left batch through the primary key (`get_many`) or a key prefix or secondary index (one `scan_many`), in concurrent calls of 256 keys, and become hash joins when the left input is not small; 10,000 lookups into a million-row table went from 1.45 s to 54 ms (ADR-0033).

### batched-row-writes: Write DML rows in batches
- **Origin:** E4
- **Why:** DML calls the row layer once per row, and each call makes its own constraint-check reads. A batch API in the row layer could probe all keys of a batch in one `get_many` and buffer its writes together.
- **When:** Milestone F3 (`design/mvp-completion.md`). Originally: when bulk inserts or large updates matter.
- **Size:** About 3 days, mostly in the storage row layer.
- **Status:** done (F3): `rows::insert_many`, `update_many` and `delete_many` read a batch's constraint-check keys in one `get_many` (two for updates) and otherwise behave exactly like single-row writes; DML makes one call per input batch. Inserting, updating and deleting a million rows went from 17.0, 21.2 and 72.6 s to 6.4, 10.3 and 29.4 s (ADR-0033).

### incremental-aggregates: Accumulate sum, min, max and bool aggregates incrementally
- **Origin:** E3 (ADR-0028 phase 1)
- **Why:** Aggregates other than plain `count` keep every argument tuple until the group finishes, so memory grows with the input. Exact incremental accumulators (integer sums in `i128`, min and max) would make it proportional to the number of groups; float sums need care to stay identical to the interpreter.
- **When:** Milestone F3 (`design/mvp-completion.md`). Originally: when large groups show up. Since E5, kept argument tuples count against the query's memory budget, so large groups fail cleanly rather than exhaust the node.
- **Size:** About 2 days.
- **Status:** done (F3): `count`, `sum`, `avg`, `min`, `max`, `bool_and` and `bool_or` fold rows into fixed-size states exactly as the interpreter computes them, float sums included; only `DISTINCT` aggregates keep tuples. A million-row aggregation into 1,000 groups went from 280 MiB reserved to 0.7 MiB, and from 2.2 s to 0.9 s (ADR-0033).

### default-memory-budgets: Choose default per-query and per-node memory budgets
- **Origin:** ADR-0027 open questions, `executor-design.md`
- **Why:** Queries that exceed the budget fail rather than spill, so the defaults decide which real queries succeed. They need numbers for the container sizes we expect.
- **When:** Milestone F3 (`design/mvp-completion.md`). Originally: before production deployment, after measuring operators on realistic data. E5 kept the 256 MiB per-query default and configures no node pool unless asked (ADR-0027).
- **Size:** About 1 day of measurement, plus an ADR update.
- **Status:** done (F3): the per-query default stays 256 MiB and the recommended node pool is a quarter of the memory left after storage's caches, since resident memory runs up to 2.6 times ahead of reservations for hash tables and about 4 times for a finishing sort (ADR-0027, ADR-0033).
