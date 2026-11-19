---
id: 0028
title: Physical Operators, Planning and EXPLAIN
status: implemented  # proposal | accepted | implemented | superseded
tags: [executor, planner, observability, graphql]
feature-flag:
created: 2026-11-17
accepted: 2026-11-19
implemented: 2026-11-20
spec-sections: ["executor-design.md#physical-operators", "executor-design.md#physical-planning-mvp", "executor-design.md#nest-and-graphql-output", "executor-design.md#observability"]
superseded-by:
---

# ADR-0028: Physical Operators, Planning and EXPLAIN

## Status

Implemented

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

- [x] **Phase 1: Joins, aggregation, sort, top-N, set operations, Values** - E3
  - *As implemented:* one `Join` operator serves all six kinds: equality conjuncts between leakproof expressions of each side become hash keys (NULL keys never match), the rest of the condition is a residual evaluated with the kernels on key-matching pairs in chunks, and with no keys it loops over all pairs. The build side is always the right input, chosen at plan time. Semi and anti joins stop at a row's first passing pair.
  - `Apply` runs the inner plan once per outer row with that row's columns visible as constants (`ExecContext::correlated`); semi and anti applies stop at the first inner row, and single applies fail on the second.
  - `Aggregate` is hash-based. Plain `count` accumulates as rows arrive; other aggregates keep their argument tuples and are finished by the interpreter's `aggregate_values`, so results (including float summation order within a group) match exactly. Incremental accumulators are future work. `StreamAggregate` is not built: nothing produces sorted input yet.
  - `Sort` uses the interpreter's `compare_sort_keys`, stably; a limit directly above makes it a top-N that compacts to the rows the limit can return. `Distinct`, `UNION`, `INTERSECT` and `EXCEPT` hash rows by value. `Values` may read an enclosing row.
  - Arrow fields are all nullable, since outer joins and applies pad NOT NULL columns with NULLs.
  - *Testing:* a differential test runs 3,000 random plans over three stored tables, optimized or not, against real storage and the interpreter, with every join and apply kind, aggregates with `DISTINCT` and `FILTER`, set operations, `VALUES`, subqueries and top-N; rows compare as bags, and in order under a limit over a total sort. Planted bugs in outer-join padding (left and right), semi joins, NULL key matching, empty aggregation, aggregate filters, top-N offsets, `EXCEPT ALL`, left-apply padding and `DISTINCT` are each caught.
- [x] **Phase 2: Index scans and lookups** - E4
  - *As implemented:* see [ADR-0023](0023-leaf-operators-and-storage-pushdown.md) phase 2. The heuristic prefers a key whose every column is equal and unique (one row), then the longest equality prefix, then a range, and the primary key on ties. The E3 differential test now runs over indexed tables and generates key ranges, so about 15% of its plans scan by an index and 15% by a primary key range.
- [x] **Phase 3: Nest, Unnest, EXPLAIN ANALYZE** - E5
  - *As implemented:* `Unnest` evaluates its array expression for a whole batch. The list offsets drive one `take` that repeats the input columns and another that gathers the elements. A record element gives one column per field, all NULL for a NULL record. NULL and empty arrays give no rows.
  - `Nest` keeps each batch's evaluated keys and fields as Arrow arrays and assigns rows to groups in first-seen order. It then builds the key columns and a list-of-struct column in two `take`s, with one list offset per group. Records keep input order within a group, so a sort below orders each nested list. Without keys there is always exactly one row.
  - Nested Arrow fields (list items, record fields) are always nullable, like top-level ones, because outer joins pad nested values too.
  - `EXPLAIN` (`explain`) prints one operator per line, indented under its parent. Each line names the algorithm: `TableScan`/`IndexScan` with their key bounds, pushed and policy conjuncts; `HashJoin`/`NestedLoopJoin` with keys and residual; `HashAggregate`; `Sort`/`TopN`; and so on. It also shows where each `PolicyFilter` sits; what non-admins see is described in [ADR-0025](0025-runtime-security-enforcement-in-executor.md).
  - `EXPLAIN ANALYZE` (`explain_analyze`) runs the plan, discarding rows, with `Metrics` that wrap every operator's stream. The metrics cover rows, non-empty batches and loops (runs, for operators inside an `Apply`), and elapsed time including inputs, as Postgres reports it. Scans add storage calls and the bytes of the values they read, and blocking operators their peak reserved memory ([ADR-0027](0027-query-memory-management.md)). Physical operators carry a preorder `id`, which indexes the metrics.
  - *Testing:* the E3 differential test now generates `Unnest` over arrays of expressions (including NULL and empty arrays and raising elements). It also generates `Nest` unnested again, grouped by keys or per outer row through an apply (the GraphQL shape). In 15% of plans the whole result is nested, and lists compare as bags. About 1,100 of its 3,000 plans contain each operator.
  - A targeted test checks NULL records, NULL keys, keyless nests of nothing and ordered lists, row for row against the interpreter.
  - `EXPLAIN ANALYZE` is checked for exact row counts per operator, storage calls, peak memory, and loops of an apply's inner scan.
  - Planted bugs are each caught: an element dropped, NULL record fields kept, a missing empty group, reversed lists, and loops not counted.

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
