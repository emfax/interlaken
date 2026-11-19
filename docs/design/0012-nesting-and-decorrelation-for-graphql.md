---
id: 0012
title: Nesting and Decorrelation for GraphQL
status: implemented  # proposal | accepted | implemented | superseded
tags: [ir, graphql, optimizer]
feature-flag:
created: 2026-11-17
accepted: 2026-11-19
implemented: 2026-11-19
spec-sections: ["ir-design.md#nesting-and-graphql"]
superseded-by:
---

# ADR-0012: Nesting and Decorrelation for GraphQL

## Status

Implemented

## Summary

GraphQL's tree-shaped results are expressed in the IR with `Apply` (dependent join) and `Nest` (collect a group's rows into a typed array of records), with `Unnest` as the inverse. The optimizer decorrelates `Apply` into joins plus grouping (Neumann and Kemper), so nested GraphQL queries run as a handful of joins instead of one query per parent. `Apply` that can't be decorrelated runs as a nested-loop join.

## Context

GraphQL returns trees; relational algebra returns flat rows. A naive translation issues one child query per parent row (the N+1 problem). Correlated SQL subqueries and LATERAL have the same shape. The IR needs a representation that captures nesting directly and a rewrite that turns it into set-oriented plans.

## Decision

- A GraphQL field selecting children becomes `Apply(outer = parent rows, inner = child query correlated on the foreign key)`, with `Nest` collecting each parent's children into an array of records.
- **Nest output is typed, not jsonb.** `Nest` produces `array(record(...))` values, the IR's internal collection types ([ADR-0010](0010-typed-relational-ir.md)), represented at run time as Arrow list-of-struct columns ([ADR-0028](0028-physical-operators-planning-and-explain.md)). Serializers (the GraphQL front end, later pgwire) convert them to JSON at the edge. Types survive through the plan, nested values are cheaper to build than JSON text, and operators above `Nest` can still inspect children.
- SQL subqueries (scalar, EXISTS, IN, ANY/ALL) are lowered to `Apply` during resolution ([ADR-0010](0010-typed-relational-ir.md)).
- **Decorrelation** following Neumann and Kemper, *Unnesting Arbitrary Queries*, rewrites `Apply` into joins plus grouping wherever possible. It is phase 2 of the optimizer ([ADR-0014](0014-rule-based-optimizer.md)).
- `Apply` that can't be decorrelated stays and runs as a `NestedLoopJoin` in the executor ([ADR-0028](0028-physical-operators-planning-and-explain.md)).
- `Unnest(input, expr)` expands an array into rows.
- The GraphQL schema (types from tables, relationship fields from foreign keys, mutations from DML) belongs to the GraphQL front end and is not specified here.

**Decorrelation as implemented (I3).** Rules run bottom-up, to a fixpoint:
- **Uncorrelated.** An apply whose inner side does not reference the outer side becomes a join on `true`. A single apply does so only when the inner side yields at most one row.
- **Correlated filters.** For semi, anti, inner and left applies, correlated conjuncts are pulled out of the inner side into the join condition. They pass through filters, projections (which expose the columns the conjuncts need) and sorts, and through `DISTINCT` where row counts do not matter (semi and anti).
- **Scalar aggregates.** An apply over an aggregate without keys, correlated only by equalities, becomes a left join to the aggregate grouped by the correlated inner expressions. References to `count` become `coalesce(count, 0)`, which avoids the count bug.
- **Nesting.** An apply over a keyless `Nest` becomes a left join to a `Nest` keyed by the correlation, with `coalesce(nested, (cast (array) T))`, so parents without children get an empty array. Element order follows the sorted input.
- **EXISTS as a value.** The left apply of one marker row that the resolver produces becomes a left join to `DISTINCT` correlation keys.
- **What stays an apply.** `NOT IN`, `ALL` and `ANY` outside filter conjuncts are lowered to aggregates over a comparison with the outer row, and those stay as applies (nested loops), as in Postgres. The same goes for correlation by non-equality, or through operators the rules do not cross (limits, keyed aggregates, joins).

To express the empty array, the IR gained an array constructor expression, `(array e...)`. An empty one is written under a cast, `(cast (array) T)`, as a typed NULL is.

## Implementation Phases

- [x] **Phase 1: Nest/Unnest/Apply operators** - I1
- [x] **Phase 2: Decorrelation rules** - I3

## Consequences

### Positive
- Nested GraphQL queries avoid N+1 query patterns.
- One mechanism serves both GraphQL nesting and SQL correlated subqueries.

### Negative
- Some correlated shapes won't decorrelate and fall back to nested loops.
- Every serializer must convert list-of-struct values to its output format.

## Open Questions

- None. (Nest output format resolved: typed Arrow list-of-struct, JSON at serialization.)

## Future Work

- GraphQL front end and schema generation.

## References

- `ir-design.md` — Nesting and GraphQL; Risks (decorrelation coverage)
- T. Neumann, A. Kemper, *Unnesting Arbitrary Queries*
- [ADR-0010](0010-typed-relational-ir.md), [ADR-0014](0014-rule-based-optimizer.md), [ADR-0028](0028-physical-operators-planning-and-explain.md)
