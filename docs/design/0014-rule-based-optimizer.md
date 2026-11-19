---
id: 0014
title: Rule-Based Optimizer for the MVP
status: implemented  # proposal | accepted | implemented | superseded
tags: [ir, optimizer]
feature-flag:
created: 2026-11-17
accepted: 2026-11-19
implemented: 2026-11-19
spec-sections: ["ir-design.md#optimizer-mvp-scope"]
superseded-by:
---

# ADR-0014: Rule-Based Optimizer for the MVP

## Status

Implemented

## Summary

The MVP optimizer is rule-based rewriting in fixed phases, all subject to the security barrier rules. Cost-based join ordering, statistics and a Cascades-style memo come later; plan nodes carry physical properties now so the cost-based planner can be added without changing the IR.

## Context

The MVP needs decorrelation and pushdown to be usable (especially for GraphQL and storage pushdown into `ScanRequest`), but has no statistics. A cost-based planner is a large investment and depends on statistics that don't exist yet. Every rewrite must respect the leakproof barrier rules or it can leak data.

## Decision

Rewrites run in fixed phases:

1. Simplification: constant folding (immutable functions only), boolean normalization, removal of always-true filters.
2. Decorrelation of `Apply` into joins and aggregates ([ADR-0012](0012-nesting-and-decorrelation-for-graphql.md)).
3. Predicate pushdown, stopping at barriers for non-leakproof predicates ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md)).
4. Projection pruning down to scans.
5. Limit pushdown through projections and into scans where order allows.

Function properties come from the catalog ([ADR-0011](0011-function-property-catalog.md)). Plan nodes carry physical properties (sort order now; distribution once ranges exist) in the style of Calcite's traits.

### As implemented (I3)

`interlaken_ir::optimize::optimize` takes a resolved, verified plan and returns one that verifies under the same options and returns the same rows as a bag. Like Postgres, it may raise an error the input would not have raised, when a moved predicate meets rows that earlier operators would have discarded. The phases:

1. **Simplification.**
   - Constant subexpressions over immutable functions are evaluated with the reference interpreter. Ones that would raise an error, such as `1/0`, are left for run time.
   - `AND`/`OR` are flattened and their `true`/`false` operands absorbed, and `NOT NOT x` becomes `x`.
   - Always-true filters are removed and stacked filters merged. A filter never merges into a `PolicyFilter`.
   - This phase runs again after decorrelation and after pushdown.
2. **Decorrelation**, run to a fixpoint (see [ADR-0012](0012-nesting-and-decorrelation-for-graphql.md)).
3. **Predicate pushdown.** Conjuncts move:
   - through projections (substituting the projected expressions), sorts and `DISTINCT`;
   - to the outer side of applies, and to the sides of joins: only the preserved side of an outer join, and into the condition when they reference both sides of an inner join;
   - below aggregate and nest grouping keys, into both inputs of set operations, and into the input of `UNNEST`.
   Join conditions that reference one side move into it where the join kind allows. A conjunct moves below a `PolicyFilter` only if every function, operator and cast in it is leakproof (judged from the catalog and the operand types), nothing moves below a `WriteCheck`, and volatile conjuncts stay put.
4. **Projection pruning.** Unread outputs are removed from projections, and unread aggregate calls from aggregates. Operators whose output is consumed positionally or as whole rows are left alone: the plan's output, set operation and insert inputs, and `DISTINCT` inputs. Which columns a scan reads is derived from the remaining references at physical planning.
5. **Order.** Each relation's output order is derived from the plan: a sort defines it, and filters, limits and the security operators preserve it. A sort over input already in that order is removed, as is a sort directly below another sort, and a limit moves below a projection.

Joins keep their written order: without statistics, reordering would be guesswork.

## Implementation Phases

- [x] **Phase 1: Optimizer rules and decorrelation** - I3

## Consequences

### Positive
- Simple, predictable rewrites that are easy to test differentially.
- Physical traits keep the door open for cost-based planning without IR changes.

### Negative
- No cost-based join ordering; join order largely follows the query shape.
- Barrier rules limit pushdown of non-leakproof predicates.

## Open Questions

- None recorded in the source.

## Future Work

- Cost-based join ordering, statistics, Cascades-style memo.
- Distribution traits once ranges exist.

## References

- `ir-design.md` — Optimizer (MVP scope)
- [ADR-0011](0011-function-property-catalog.md), [ADR-0012](0012-nesting-and-decorrelation-for-graphql.md), [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0023](0023-leaf-operators-and-storage-pushdown.md), [ADR-0028](0028-physical-operators-planning-and-explain.md)
- Apache Calcite traits; Cascades optimizer framework
