---
id: 0027
title: Query Memory Management
status: implemented  # proposal | accepted | implemented | superseded
tags: [executor, memory]
feature-flag:
created: 2026-11-17
accepted: 2026-11-20
implemented: 2026-11-20
spec-sections: ["executor-design.md#memory-management", "executor-design.md#summary"]
superseded-by:
---

# ADR-0027: Query Memory Management

## Status

Implemented

## Summary

Each query has a memory budget (default 256 MiB), tracked through Arrow buffer sizes for blocking operators. Streaming operators are effectively exempt. Exceeding the budget fails the query with a clear error naming the operator; there is no spilling in the MVP. A node-level cap bounds total memory across concurrent queries.

## Context

Blocking operators (hash tables, sorts, aggregation state, `Nest` accumulators) can grow without bound, while streaming operators hold only a few batches. The system runs in containers, where exhausting memory kills the whole node. Spilling to disk avoids failures but adds substantial complexity. The MVP success criterion is that a query over 10 million rows runs in bounded memory for streaming operators and fails cleanly when a blocking operator exceeds its budget.

## Decision

- **Per-query budget,** default 256 MiB, tracked through Arrow buffer sizes for blocking operators: hash tables, sort buffers, aggregation state and `Nest` accumulators.
- **Streaming operators** (scan, filter, project, limit, probe side of a hash join) hold only a few batches and don't count against the budget in practice.
- **Exceeding the budget** fails the query with a clear error naming the operator. No spilling.
- **Node-level cap.** A global limit across concurrent queries prevents many medium queries from exhausting the container. When an allocation would cross it, the query making that allocation fails with a clear out-of-memory error, as with the per-query budget.
- Peak memory is reported per operator in `EXPLAIN ANALYZE` ([ADR-0028](0028-physical-operators-planning-and-explain.md)).

**Testing.** Budget overruns fail cleanly; cancelled queries release resources.

## Implementation Phases

- [x] **Phase 1: Per-query budget and node-level cap** - E5
  - *As implemented:* each query has a `QueryMemory` (default `DEFAULT_QUERY_BUDGET`, 256 MiB), optionally drawing on a node-wide `MemoryPool` shared by concurrent queries. Blocking operators hold a reservation, released when it is dropped, so a finished, failed or cancelled query returns everything. The operators and their error labels:
    - `HashJoin`: the build side's batches, then its key index.
    - `HashAggregate`: groups and kept argument tuples.
    - `Sort` and `TopN`: buffered batches; a top-N shrinks its reservation as it compacts.
    - `Distinct`: rows seen.
    - `SetOp`: counts and rows seen.
    - `Nest`: evaluated keys, fields and groups.
    - `ModifyTable`: the primary keys an update or delete has changed.
  - Sizes are estimates: Arrow buffer sizes for batches, and value sizes plus per-entry overhead for hash tables (made to match the heap in phase 3). Operators reserve as they read, so a query fails before buffering its whole input.
  - Crossing either limit fails the query with `ExecError::MemoryExceeded` (SQLSTATE `53200`), for example `Sort exceeded the query memory budget` or `... the node memory limit`.
  - `ExecContext::with_memory` and `Transaction::with_memory` set the budget and pool. No node pool exists unless one is configured. Peak memory per operator appears in `EXPLAIN ANALYZE`.
  - *Testing:* each blocking operator is checked against a 64 KiB budget over 5,000 rows. It must fail with its own name, having read less than its whole input, and leave nothing reserved. The same queries succeed at the default budget. Streaming operators and a top-N over the whole table run in budgets where a full sort fails. Two sorts that fit their own budgets fail together at the node pool, and the second succeeds once the first is cancelled. Cancelling a hash join or a sort returns its memory. A statement over its budget aborts its transaction. Planted bugs (a reservation not released, the pool ignored, a join building without reserving) are each caught.

- [x] **Phase 2: Default budgets from measurements** - F3 ([ADR-0033](0033-executor-performance.md))
  - *As implemented:* the executor benchmarks (`crates/engine/benches/executor.rs`, one million rows of eight columns) measured each blocking operator's peak reservation and the process's resident growth:

    | Operator (1M rows) | Peak reserved | Resident growth |
    | --- | --- | --- |
    | Hash join building 1M rows | 132 MiB | 340 MiB |
    | Hash join building 100k rows | 9.7 MiB | 6–50 MiB |
    | Sort of 1M rows | 62 MiB | 15–240 MiB |
    | Top-N (10 rows) | 0.1 MiB | 0 |
    | Aggregation into 100k groups | 14 MiB | 2 MiB |
    | Aggregation into 1,000 groups, before F3's incremental aggregates | 280 MiB | 160 MiB |
    | The same, after | 0.7 MiB | 0 |
    | Scalar aggregation, before / after | 226 MiB / 0 | 603 MiB / 0 |

    So 256 MiB holds a sort of about four million such rows or a hash build of about two million, and every streaming or grouped query the benchmarks run. At ten million rows a full sort fails at it, cleanly, with the resident set growing 205 MiB (`crates/exec/tests/ten_million.rs`). The per-query default stays **256 MiB**.
  - Resident memory runs ahead of reservations: up to **2.6 times** for hash tables (row keys as `Datum`s cost more than estimated), and up to about **4 times** for a sort at the moment it finishes, when its buffered batches, their concatenation and the sorted copy exist together (backlog `memory-estimates`). The recommended node pool (`EngineOptions::node_memory_limit`) is therefore **a quarter of the memory left after storage's caches**: for a 4 GiB container with 1 GiB of fjall cache, 768 MiB, which admits three concurrent queries at their full default budget. Embedded use still configures no pool by default, since the engine can't know its container. *Superseded by phase 3, which closed the gap: the recommendation is now half.*

- [x] **Phase 3: Reservations that match the heap** - backlogs `memory-estimates` and `slice-memory-accounting`
  - *As implemented:* reservations are now estimates of the heap an operator holds, checked against a counting global allocator (`crates/test-alloc`, a dev-only crate and the workspace's only `unsafe` code, which counts the chunks glibc's `malloc` hands out):
    - Values count their inline size plus every heap allocation they own, by capacity, rounded up to the allocator's chunk (`memory::alloc_bytes`: request plus an 8-byte header, rounded to 16, at least 32). A 13-byte string costs 32 bytes, not 13.
    - Hash tables count their buckets and control bytes from the table's capacity (hashbrown's power-of-two layout), and while a table doubles, the old and new tables together (`Reservation::reserve_entry`).
    - Batches count their Arrow buffers, or, for a slice of much larger buffers, only the slice's own rows, following list offsets into their values (`memory::batch_bytes`). Slices of one batch then add up to that batch.
  - Operators hold less, so the heap shrank as the estimates grew:
    - Row keys hold exactly their values (collecting through a `Result` had left room for four).
    - A hash join chains equal keys through one `next` array instead of a vector of rows per key, and evaluates build keys a batch at a time. It reserves for the batches read and their concatenation while both exist.
    - An aggregation holds each key once (it was cloned into the output order too) and all accumulators in one vector. It finishes and emits groups a batch at a time, so the whole output never exists at once.
    - A sort keeps its input batches as they arrive, with their key values in one flat vector, sorts row numbers, and gathers each output batch from the kept batches (`interleave`). It never concatenates the input or holds a sorted copy. It reserves for the key values and the sort's scratch space while sorting and releases them afterwards.
    - `Nest` concatenates and gathers one column at a time, releasing each column's parts as it goes.
  - Sort and aggregation outputs are therefore compact batches, not slices of one buffer. `Nest` output is still sliced, and is counted by its slices' own rows. For slices, accounting won over compacting: it costs a pass over each column's offsets and copies nothing, while compacting would copy every buffered slice.
  - Counting-allocator measurements, 100,000 rows of four columns (`crates/exec/tests/memory_estimates.rs`). Heap is the peak heap growth during the query, less a plain scan's:

    | Operator | Before: reserved / heap | After: reserved / heap |
    | --- | --- | --- |
    | Hash join, text key | 12.7 / 34.3 MiB (0.37) | 16.0 / 16.1 MiB (1.00) |
    | Hash join, integer key | 11.1 / 31.2 MiB (0.36) | 13.1 / 13.0 MiB (1.01) |
    | Sort, text key | 3.9 / 27.5 MiB (0.14) | 11.7 / 11.4 MiB (1.02) |
    | Sort, two keys | 3.9 / 24.4 MiB (0.16) | 14.6 / 14.3 MiB (1.02) |
    | Aggregation into 100,000 groups | 36.6 / 48.2 MiB (0.76) | 26.1 / 26.9 MiB (0.97) |
    | Aggregation keeping tuples (`count(DISTINCT)`) | 5.4 / 15.9 MiB (0.34) | 6.9 / 6.9 MiB (1.00) |
    | `Distinct` | 8.4 / 19.8 MiB (0.42) | 10.8 / 10.6 MiB (1.01) |
    | `INTERSECT ALL` | 9.1 / 20.9 MiB (0.44) | 11.8 / 11.8 MiB (1.00) |
    | `Nest` into 100 groups | 3.5 / 8.2 MiB (0.43) | 5.0 / 4.7 MiB (1.07) |
    | Sort over a sort | 377 / 27.5 MiB (13.7) | 12.8 / 12.6 MiB (1.02) |
    | Hash join building from a sort | 377 / 34.2 MiB (11.0) | 16.0 / 16.0 MiB (1.00) |
    | Sort over a `Nest` of 100,000 groups | 386 / 33.2 MiB (11.6) | 19.0 / 18.1 MiB (1.05) |

    Every blocking operator now reserves within 7% of the heap it grows, either way; before, reservations covered between a seventh and three quarters of it, and plans buffering another blocking operator's output overcounted elevenfold or more.
  - The engine benchmarks (one million rows, eight columns) reserve, before → after: hash join building 100k rows 9.7 → 11.7 MiB (resident growth 19.1 → 15.5 MiB); building 1M rows (`index_nl_pk_all`) 132 → 204 MiB (resident 368 → 173 MiB); sort 62 → 132 MiB (resident 245 → 68 MiB); aggregation into 100k groups 13.7 → 11.0 MiB (resident 1.5 → 0.3 MiB); `UPDATE` of every row 69 → 117 MiB. Holding less also made them faster (median, same machine): the hash join 1.28 → 1.19 s, the million-row build 3.32 → 2.63 s, the sort 1.83 → 1.57 s, the aggregation 1.09 → 0.92 s. Resident growth is a lower bound on the heap a query uses, since `malloc` reuses pages freed by earlier work, so it can now sit below the reservation. An `UPDATE`'s resident growth (444 → 458 MiB) is mostly storage's write set, bounded per transaction by `StorageOptions::max_write_set_bytes`, not by the query budget.
  - Budgets now mean what they say, so they admit less than before: 256 MiB holds a sort of about two million of the benchmark's rows (it was "four million", while the heap really grew by four times the reservation) or a hash build of about 1.2 million. The per-query default stays **256 MiB**. At ten million rows a full sort still fails cleanly at it (`ten_million.rs`), now after 1.5 s with 160 MiB of resident growth (it was 4.2 s and 205 MiB): it reaches its budget sooner because the budget now counts the key values it holds.
  - **Node pool recommendation, revised:** **half of the memory left after storage's caches** (it was a quarter). Reservations now track the heap within about 10%; the rest of the allowance covers what no reservation counts: allocator fragmentation and retained free pages, the few batches each streaming operator has in flight, results the engine collects for a session (backlog `engine-streaming-results`) and a DML statement's storage write set (bounded separately by `max_write_set_bytes`). For a 4 GiB container with 1 GiB of fjall cache that is 1.5 GiB, which admits six concurrent queries at their full default budget. Embedded use still configures no pool by default.
  - *Testing:* the counting-allocator test above runs 13 plans (including a hash join whose peak is its build batches and their concatenation) and requires each reservation within 1.2 times of the heap, either way, then runs a sort over a sort, a hash join building from a sort and a sort over a sliced `Nest` output in a 64 MiB budget each previously exceeded. Unit tests check the hash-table bucket model and the vector growth model against `std`'s as they grow, that slices of plain, string and list-of-struct arrays add up to their rows' bytes, and the allocator-chunk and capacity rules. The memory tests' budgets moved where estimates grew: a top-N now needs 256 KiB rather than 128 KiB (two batches and their key values, which the old estimate left out), and the node pool test sizes the pool from a sort's peak (while sorting) and its holding (while emitting), which now differ. Twelve planted bugs were tried; eleven are caught (a table that never doubles, old tables never released, the allocator header dropped, string length instead of capacity, slices counted whole, lists counting their whole child, sort key values unreserved, join keys unreserved, `Nest`'s concatenation unreserved, aggregation state growth unreserved, the join's concatenation unreserved). Omitting the sort's scratch space (8–14% of its peak) stays within the tolerance and is not caught.

## Consequences

### Positive
- Predictable memory use; one large query can't take down the node.
- Simple to implement and reason about.

### Negative
- Large sorts and joins fail at the budget instead of degrading. Acceptable for the target workload size; revisit with real usage.

## Open Questions

- None. *The default budgets were chosen in F3 (phase 2); the node pool recommendation was revised once reservations matched the heap (phase 3).*

## Future Work

- Spilling sorts and hash tables to disk.
- Admission control (queueing new queries for memory) once memory estimates exist.

## References

- `executor-design.md` — Memory management, Summary
- [ADR-0022](0022-vectorized-arrow-stream-executor.md), [ADR-0028](0028-physical-operators-planning-and-explain.md)
