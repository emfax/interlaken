---
id: 0027
title: Query Memory Management
status: accepted  # proposal | accepted | implemented | superseded
tags: [executor, memory]
feature-flag:
created: 2026-11-17
accepted: 2026-11-20
implemented:
spec-sections: ["executor-design.md#memory-management", "executor-design.md#summary"]
superseded-by:
---

# ADR-0027: Query Memory Management

## Status

Accepted

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
  - Sizes are estimates: Arrow buffer sizes for batches, and value sizes plus per-entry overhead for hash tables. Operators reserve as they read, so a query fails before buffering its whole input.
  - Crossing either limit fails the query with `ExecError::MemoryExceeded` (SQLSTATE `53200`), for example `Sort exceeded the query memory budget` or `... the node memory limit`.
  - `ExecContext::with_memory` and `Transaction::with_memory` set the budget and pool. No node pool exists unless one is configured. Peak memory per operator appears in `EXPLAIN ANALYZE`.
  - *Testing:* each blocking operator is checked against a 64 KiB budget over 5,000 rows. It must fail with its own name, having read less than its whole input, and leave nothing reserved. The same queries succeed at the default budget. Streaming operators and a top-N over the whole table run in budgets where a full sort fails. Two sorts that fit their own budgets fail together at the node pool, and the second succeeds once the first is cancelled. Cancelling a hash join or a sort returns its memory. A statement over its budget aborts its transaction. Planted bugs (a reservation not released, the pool ignored, a join building without reserving) are each caught.

## Consequences

### Positive
- Predictable memory use; one large query can't take down the node.
- Simple to implement and reason about.

### Negative
- Large sorts and joins fail at the budget instead of degrading. Acceptable for the target workload size; revisit with real usage.

## Open Questions

- Default per-query and per-node budgets for the container sizes we expect. E5 keeps the 256 MiB per-query default and sets no node pool unless one is configured (backlog `default-memory-budgets`).

## Future Work

- Spilling sorts and hash tables to disk.
- Admission control (queueing new queries for memory) once memory estimates exist.

## References

- `executor-design.md` — Memory management, Summary
- [ADR-0022](0022-vectorized-arrow-stream-executor.md), [ADR-0028](0028-physical-operators-planning-and-explain.md)
