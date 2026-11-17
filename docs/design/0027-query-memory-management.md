---
id: 0027
title: Query Memory Management
status: proposal  # proposal | accepted | implemented | superseded
tags: [executor, memory]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["executor-design.md#memory-management", "executor-design.md#summary"]
superseded-by:
---

# ADR-0027: Query Memory Management

## Status

Proposal

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

- [ ] **Phase 1: Per-query budget and node-level cap** - E5

## Consequences

### Positive
- Predictable memory use; one large query can't take down the node.
- Simple to implement and reason about.

### Negative
- Large sorts and joins fail at the budget instead of degrading. Acceptable for the target workload size; revisit with real usage.

## Open Questions

- Default per-query and per-node budgets for the container sizes we expect.

## Future Work

- Spilling sorts and hash tables to disk.
- Admission control (queueing new queries for memory) once memory estimates exist.

## References

- `executor-design.md` — Memory management, Summary
- [ADR-0022](0022-vectorized-arrow-stream-executor.md), [ADR-0028](0028-physical-operators-planning-and-explain.md)
