---
id: 0014
title: Rule-Based Optimizer for the MVP
status: proposal  # proposal | accepted | implemented | superseded
tags: [ir, optimizer]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["ir-design.md#optimizer-mvp-scope"]
superseded-by:
---

# ADR-0014: Rule-Based Optimizer for the MVP

## Status

Proposal

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

## Implementation Phases

- [ ] **Phase 1: Optimizer rules and decorrelation** - I3

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
