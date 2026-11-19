---
id: 0025
title: Runtime Security Enforcement in the Executor
status: accepted  # proposal | accepted | implemented | superseded
tags: [executor, security]
feature-flag:
created: 2026-11-17
accepted: 2026-11-20
implemented:
spec-sections: ["executor-design.md#security-in-the-executor", "executor-design.md#observability", "executor-design.md#testing-strategy"]
superseded-by:
---

# ADR-0025: Runtime Security Enforcement in the Executor

## Status

Accepted

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

- [x] **Phase 1: PolicyFilter and WriteCheck operators, residual pushdown, end-to-end security oracle** - E5
  - *As implemented:* the planner treats a chain of filters and policy filters directly above a scan as one unit. Leakproof conjuncts on the scan's own columns, from any filter in the chain, policy residual conjuncts included, run in storage as the scan's `RowFilter`. Rows they reject never leave the storage pool, and they still enter the read set. Every other conjunct stays in its own filter, in its original order, so a `PolicyFilter` still runs before anything above it. Leakproof conjuncts can't raise, so where they run doesn't matter. Policy conjuncts may also bound the scan's key range, since the filter is always kept as a recheck. The scan keeps pushed policy conjuncts apart from the user's, for `EXPLAIN`. A residual that needs a join (attribute paths, hierarchy closures, tag sets via `Unnest`) stays in a `PolicyFilter` above the join or apply that lowering added.
  - `WriteCheck` checks each batch of DML input before the DML operator sees it, so a rejected row is never written. A check that is FALSE or NULL fails the statement with `ExecError::PermissionDenied`: SQLSTATE `42501`, message `permission denied for table docs`, with no row values. Because DML streams, an earlier batch may already be written when a later one fails. The statement savepoint ([ADR-0026](0026-statement-atomicity-and-halloween-protection.md)) rolls those writes back. The error may also differ from the interpreter's, which checks every row before writing any; for example, the executor can hit a unique violation first.
  - `EXPLAIN` without policy privilege (`ExplainOptions::show_policies` false) prints `PolicyFilter(table)` and `WriteCheck(table)` without their expressions. A governed scan hides its key values and policy conjuncts. On the path from a policy filter down to its rows, the inner side of each join or apply, which lowering added, collapses to `<policy> subplan`, and the join's condition is hidden. This redaction depends on the shape of lowered plans; a marker from lowering would make it exact (backlog `explain-policy-markers`).
  - The fixture's `docs.tags` is an array, so storage gained array columns ([ADR-0003](0003-order-preserving-tuple-encoding-and-row-format.md)).
  - *Testing:* the IR security oracle runs end to end. In 3,000 random cases (policies, data, principals, tenants and queries, with 1 to 3 rows per storage call), the secured plans, optimized or not, are planned physically and executed over real storage. They must return exactly what the unsecured query returns in the interpreter over the authorized rows, and never fail where that succeeds. About 59% of the plans push policy conjuncts into storage, and about 58% keep a `PolicyFilter`.
  - Every physical plan is also checked for the barrier rule: nothing but leakproof expressions runs below a policy filter or in a governed scan. The leak suite runs through the executor: hidden rows' errors, aggregates, `LIMIT`, `EXISTS`, nesting and unnesting. So do the USING and WITH CHECK write tests and a NULL write check. `EXPLAIN ANALYZE` confirms that only the visible row leaves storage. `EXPLAIN` is checked to hide distinctive policy literals, closure tables and lowering aliases, while keeping the user's own predicates.
  - Seven planted bugs are each caught: dropped policy pushdown, filter stages rebuilt in the wrong order, non-leakproof conjuncts pushed into storage, write checks passing NULL or never failing, and `EXPLAIN` showing residuals or policy subplans.

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
