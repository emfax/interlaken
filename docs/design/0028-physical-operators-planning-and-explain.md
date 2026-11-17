---
id: 0028
title: Physical Operators, Planning and EXPLAIN
status: proposal  # proposal | accepted | implemented | superseded
tags: [executor, planner, observability, graphql]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["executor-design.md#physical-operators", "executor-design.md#physical-planning-mvp", "executor-design.md#nest-and-graphql-output", "executor-design.md#observability"]
superseded-by:
---

# ADR-0028: Physical Operators, Planning and EXPLAIN

## Status

Proposal

## Summary

The MVP defines a fixed set of in-memory physical operators, chosen from optimized IR by a heuristic physical planner (no statistics yet). `Nest` produces one result row per GraphQL root object with children inline. `EXPLAIN` shows the physical plan including pushdown and barrier placement; `EXPLAIN ANALYZE` adds per-operator metrics.

## Context

The rule-based optimizer ([ADR-0014](0014-rule-based-optimizer.md)) emits logical IR ([ADR-0010](0010-typed-relational-ir.md)); something must choose concrete algorithms and access paths. Statistics and cost-based planning are post-MVP, so choices rely on heuristics. GraphQL queries are decorrelated into joins plus `Nest` ([ADR-0012](0012-nesting-and-decorrelation-for-graphql.md)) and need nested output without per-parent queries. Users and developers need to see what was pushed into storage and where security barriers sit.

## Decision

**Physical operators.**

| Operator | Notes |
| --- | --- |
| `TableScan`, `IndexScan`, `IndexLookup` | Leaves ([ADR-0023](0023-leaf-operators-and-storage-pushdown.md)) |
| `Filter` | Vectorized predicate, producing a selection vector |
| `Project` | Vectorized expressions |
| `HashJoin` | inner, left, right, full, semi, anti; build side chosen at plan time |
| `NestedLoopJoin` | `Apply` that couldn't be decorrelated, and non-equi joins; inner side re-evaluated or cached per outer batch |
| `HashAggregate` / `StreamAggregate` | Stream aggregation when input is sorted on the group keys |
| `Sort` / `TopN` | In-memory; `TopN` for `ORDER BY ... LIMIT` |
| `Limit` | Stops pulling from its input once satisfied |
| `Union` / `Intersect` / `Except` | Hash-based for distinct forms |
| `Values` | Literal rows |
| `Nest` | Builds typed Arrow list-of-struct columns per group |
| `Unnest` | Expands arrays into rows |
| `Insert` / `Update` / `Delete` | DML sinks ([ADR-0026](0026-statement-atomicity-and-halloween-protection.md)) |
| `PolicyFilter`, `WriteCheck` | Security ([ADR-0025](0025-runtime-security-enforcement-in-executor.md)) |

**Physical planning (heuristic).**

- Equality on a unique index becomes `IndexLookup`; a range on an index prefix becomes `IndexScan`; otherwise `TableScan`.
- Push filters, projections and limits into `ScanRequest`, subject to the barrier rules.
- `HashJoin` for equi-joins with the smaller estimated side as build side (simple heuristics until statistics exist), fixed at plan time for the MVP; `NestedLoopJoin` otherwise.
- `StreamAggregate` and merge-friendly plans when input order allows; otherwise hash.

**Nest and GraphQL output.**

- `Nest` collects each group's child rows into typed Arrow list-of-struct columns (matching the IR's internal array and record types), so a decorrelated GraphQL query yields one row per root object with children inline. Serializers such as the GraphQL front end convert these to JSON at the edge; no second round of queries.
- Ordering and limits inside nested fields (`children(first: 10, orderBy: ...)`) are applied inside the group before nesting.

**Observability.**

- `EXPLAIN` prints the physical plan, including what was pushed into each `ScanRequest` and where each `PolicyFilter` sits (residuals only for principals allowed to see policy definitions).
- `EXPLAIN ANALYZE` adds per-operator metrics: rows and batches produced, elapsed time, storage calls and bytes read, and peak memory.

## Implementation Phases

- [ ] **Phase 1: Joins, aggregation, sort, top-N, set operations, Values** - E3
- [ ] **Phase 2: Index scans and lookups** - E4
- [ ] **Phase 3: Nest, Unnest, EXPLAIN ANALYZE** - E5

## Consequences

### Positive
- Covers the full MVP IR operator set with well-understood algorithms.
- GraphQL nested queries run as a handful of joins, avoiding N+1.
- `EXPLAIN` makes pushdown and barrier placement visible and testable.

### Negative
- Heuristic estimates can pick a poor build side or access path.
- All blocking operators are in-memory and bounded by [ADR-0027](0027-query-memory-management.md).
- Non-decorrelated `Apply` falls back to nested loops.

## Open Questions

- None.

## Future Work

- Merge join, window functions, spilling operators.
- Cost-based planning with statistics.
- Run-time hash join build-side selection, revisited once statistics exist.
- Query profiling UI and plan history.

## References

- `executor-design.md` — Physical operators, Physical planning, Nest and GraphQL output, Observability
- [ADR-0010](0010-typed-relational-ir.md), [ADR-0012](0012-nesting-and-decorrelation-for-graphql.md), [ADR-0014](0014-rule-based-optimizer.md), [ADR-0023](0023-leaf-operators-and-storage-pushdown.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md), [ADR-0026](0026-statement-atomicity-and-halloween-protection.md), [ADR-0027](0027-query-memory-management.md)
