---
id: 0025
title: Runtime Security Enforcement in the Executor
status: proposal  # proposal | accepted | implemented | superseded
tags: [executor, security]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["executor-design.md#security-in-the-executor", "executor-design.md#observability", "executor-design.md#testing-strategy"]
superseded-by:
---

# ADR-0025: Runtime Security Enforcement in the Executor

## Status

Proposal

## Summary

The IR defines the barrier rules; the executor enforces them at run time. `PolicyFilter` runs before any non-leakproof expression sees a row, the single-row part of the leakproof policy residual is pushed into `ScanRequest` so rows it rejects never leave the storage pool, residual errors are contained per row, and `WriteCheck` authorizes each batch of new or changed rows before it is buffered into the transaction.

## Context

Security lowering inserts `PolicyFilter` and `WriteCheck` nodes and constrains the optimizer ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md)). Plan shape alone is not sufficient: the physical planner, pushdown into storage, error handling and `EXPLAIN` output can each leak information about rows the caller can't see. Leaks happen when a user predicate runs before the policy filter and surfaces an error, side effect or log entry.

## Decision

- **Ordering.** A `PolicyFilter` runs before any operator that evaluates a non-leakproof expression on the same rows. The physical planner pushes only leakproof expressions below it.
- **Residual pushdown.** The residual is built only from leakproof sublanguage operators ([ADR-0016](0016-security-expression-sublanguage.md)), so its single-row conjuncts are pushed into `ScanRequest` as a `RowFilter` and run on the storage thread ([ADR-0023](0023-leaf-operators-and-storage-pushdown.md)). Rows rejected by that pushed part never leave the storage pool. Conjuncts that depend on a join (foreign-key attribute paths, `in` semi-joins against the closure table) run as a `PolicyFilter` above the join they need, still before any non-leakproof expression sees the rows ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md)). Filtered rows still enter the SSI read set ([ADR-0006](0006-batched-async-storage-trait.md)).
- **No leaked errors.** Residual evaluation errors are contained per row via `Try` error masks ([ADR-0024](0024-expression-evaluation-with-postgres-semantics.md)). Non-leakproof user expressions only run on rows that passed the policy.
- **Write checks.** `WriteCheck` runs on each batch of new or changed rows before they are buffered into the transaction. A failure aborts the statement with a permission error naming the table, never a row's values.
- **WASM host calls** build their scans through the same planner path, so they get the same `PolicyFilter` and pushdown ([ADR-0029](0029-wasm-udf-interface.md)).
- **EXPLAIN.** Only principals with policy-admin privilege (allowed to read the policy set) see residuals in `EXPLAIN` output. Everyone else sees an opaque `PolicyFilter(table)` node, so the plan shape is visible but the policy logic isn't ([ADR-0028](0028-physical-operators-planning-and-explain.md)).

**Testing.** The IR security oracle and leak suite ([ADR-0021](0021-ir-correctness-and-security-testing.md)) run end to end through the executor.

## Implementation Phases

- [ ] **Phase 1: PolicyFilter and WriteCheck operators, residual pushdown, end-to-end security oracle** - E5

## Consequences

### Positive
- Defence in depth: barrier rules hold in the plan and at run time.
- Pushing the residual into storage is both safer and faster: hidden rows are never copied out.
- Permission errors never reveal row contents.

### Negative
- Strict leakproof rules keep non-leakproof predicates out of scans, costing performance until more built-ins are proven leakproof.
- The planner must carry leakproof information on every expression it moves.

## Open Questions

- None.

## Future Work

- Admin-only client-supplied physical plans, which still get the residual applied inside `ScanRequest` ([ADR-0015](0015-security-core-and-policy-provider-contract.md)).
- Column masking functions.

## References

- `executor-design.md` — Security in the executor, Observability, Testing strategy
- [ADR-0006](0006-batched-async-storage-trait.md), [ADR-0011](0011-function-property-catalog.md), [ADR-0015](0015-security-core-and-policy-provider-contract.md), [ADR-0016](0016-security-expression-sublanguage.md), [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0021](0021-ir-correctness-and-security-testing.md), [ADR-0023](0023-leaf-operators-and-storage-pushdown.md), [ADR-0024](0024-expression-evaluation-with-postgres-semantics.md)
