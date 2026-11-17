---
id: 0011
title: Function Property Catalog
status: proposal  # proposal | accepted | implemented | superseded
tags: [ir, functions, security]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["ir-design.md#function-properties"]
superseded-by:
---

# ADR-0011: Function Property Catalog

## Status

Proposal

## Summary

Every function, built-in now and WASM later, declares volatility, strictness, leakproofness, determinism, cost and optional selectivity in the catalog. The optimizer and security lowering read these properties from the catalog only, never from client-supplied IR. Only administrators can mark a function leakproof.

## Context

The optimizer needs to know which functions can be folded, reordered or cached. The security barrier ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md)) needs to know which predicates may move below a `PolicyFilter` without leaking hidden rows through errors or side effects. Retried transactions ([ADR-0004](0004-optimistic-transactions-and-isolation-levels.md)) need deterministic functions. If a client could assert these properties in IR, it could bypass the barrier, so they must come from a trusted source.

## Decision

| Property | Values | Used for |
| --- | --- | --- |
| Volatility | immutable, stable, volatile | Constant folding, reordering, caching |
| Strict | yes / no | NULL in gives NULL out without calling the function |
| Leakproof | yes / no | Whether a predicate may move below a security barrier |
| Deterministic | yes / no | Required for anything run inside retried transactions |
| Cost | per-call estimate | Predicate ordering, later cost-based planning |
| Selectivity | optional estimate | Planning filters the optimizer can't see into |

- Properties are read from the catalog only. Function calls in IR reference a catalog function ID ([ADR-0010](0010-typed-relational-ir.md)); the verifier rejects any client attempt to claim a function is leakproof ([ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md)).
- Only administrators can mark a function leakproof.
- Built-in comparison and arithmetic operators on scalar types are leakproof only where they cannot raise data-dependent errors. Integer equality is leakproof; division is not.
- WASM UDFs are not implemented in the MVP, but the properties are modelled now so they slot in unchanged ([ADR-0029](0029-wasm-udf-interface.md)).

## Implementation Phases

- [ ] **Phase 1: Function catalog with properties** - I2

## Consequences

### Positive
- A single trusted source for the facts the barrier rules depend on.
- WASM UDFs can be added without changing the optimizer or security model.

### Negative
- Strict leakproof rules keep many predicates out of storage scans, costing performance. Expanding the set of built-ins proven leakproof is ongoing work.
- Every new built-in needs its properties audited, especially leakproofness.

## Open Questions

- None recorded in the source.

## Future Work

- WASM UDFs (properties modelled now).
- Extending the set of built-ins proven leakproof.

## References

- `ir-design.md` — Function properties; Risks (barrier rules versus performance)
- [ADR-0010](0010-typed-relational-ir.md), [ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md), [ADR-0014](0014-rule-based-optimizer.md), [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0029](0029-wasm-udf-interface.md)
- PostgreSQL `LEAKPROOF` and volatility categories
