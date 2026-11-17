---
id: 0018
title: PolicyFilter Barrier and WriteCheck Plan Constructs
status: proposal  # proposal | accepted | implemented | superseded
tags: [security, ir, optimizer]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["ir-design.md#plan-constructs", "ir-design.md#barrier-rules-for-the-optimizer", "ir-design.md#actions-and-granularity"]
superseded-by:
---

# ADR-0018: PolicyFilter Barrier and WriteCheck Plan Constructs

## Status

Proposal

## Summary

Security lowering inserts a privileged `PolicyFilter` above each `Get` of a policy-governed table and a privileged `WriteCheck` on DML. `PolicyFilter` is a security barrier: only leakproof predicates may move below it. Actions are per operation per table, and column-level read control rejects queries that reference forbidden columns rather than returning NULL.

## Context

A user predicate evaluated before the policy filter can leak rows the caller can't see, through an error message ("division by zero" reveals a value), a UDF that logs, or a side effect. Writes need authorization on both existing and new row versions, matching Postgres `USING` and `WITH CHECK`. Column-level control must not produce ambiguous results.

## Decision

### `PolicyFilter(input, residual)` — the security barrier

- Inserted directly above each `Get` of a policy-governed table, with the residual for (principal, read action, that table).
- `Allow` lowers to no filter. `Deny` lowers to an empty relation (the scan is skipped). `Conditional` lowers to the filter.
- It is the boundary for the leakproof rule below.

### Residual split

A `RowFilter` pushed into `ScanRequest` sees one encoded row ([ADR-0006](0006-batched-async-storage-trait.md)), so not every residual can run there. Lowering splits a `Conditional` residual into conjuncts:

- **Single-row conjuncts** (comparisons and functions over the governed table's own columns) are pushed into `ScanRequest` as a `RowFilter` and run on the storage thread.
- **Join-dependent conjuncts** (attribute paths through foreign keys, `in` semi-joins against the closure table, set operators over child tables) stay as a `PolicyFilter` placed above the semi-join or join they need, and still below every non-leakproof predicate that references the governed table's columns.

Both parts are engine-produced and leakproof; the split only changes where they run.

### `WriteCheck(input, check)` — per-row authorization on DML

- Insert: `authorize` on each new row (Postgres `WITH CHECK`).
- Update: target rows must pass the read policy for the update action (Postgres `USING`), and each new row version must pass `authorize` (`WITH CHECK`).
- Delete: target rows must pass the policy for the delete action.
- A failing row aborts the statement with a permission error that names the table, not the row's contents.

### Barrier rules for the optimizer

- A predicate may move below a `PolicyFilter` (for example, into the storage scan) **only if every function in it is leakproof** ([ADR-0011](0011-function-property-catalog.md)).
- Non-leakproof predicates stay above the barrier and only ever see rows that passed the policy.
- The policy residual itself may always move down, since it is built only from leakproof sublanguage operators ([ADR-0016](0016-security-expression-sublanguage.md)); its single-row part moves into the scan, its join-dependent part as far as its joins allow (see Residual split).
- Join reordering must keep each governed table's `PolicyFilter` between its scan and every non-leakproof predicate that references its columns, including join conditions.
- Errors raised while evaluating the residual are contained per row by `Try` and never surface.

### Actions and granularity

- **Actions:** one per operation per table: `read`, `insert`, `update`, `delete`.
- **Column-level control (MVP):** per-column read actions, enforced by projection. A column the principal can't read can't be referenced: the query fails with a permission error rather than returning NULL, so a NULL never means "hidden" and predicates on hidden columns can't behave surprisingly. Masking functions (later) are the explicit way to return redacted values.

Both nodes are privileged: the verifier rejects them in client IR ([ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md)). The executor enforces these rules at run time ([ADR-0025](0025-runtime-security-enforcement-in-executor.md)).

## Implementation Phases

- [ ] **Phase 1: PolicyFilter, WriteCheck, barrier rules, column-level actions** - I4

## Consequences

### Positive
- Leaks through errors, side effects and join order are ruled out by construction.
- Single-row residual conjuncts push into storage, so rows they reject never leave the storage pool.
- Postgres-compatible write-check semantics.

### Negative
- Non-leakproof predicates can't be pushed into scans, reducing performance.
- Rejecting forbidden column references means queries must be tailored to the principal (`SELECT *` fails if any column is hidden).
- Join-dependent residual conjuncts run in the executor, so rows they reject are read and decoded before being dropped.

## Open Questions

- None.

## Future Work

- Column masking functions returning redacted values.

## References

- `ir-design.md` — Plan constructs, Barrier rules for the optimizer, Actions and granularity
- [ADR-0006](0006-batched-async-storage-trait.md), [ADR-0011](0011-function-property-catalog.md), [ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md), [ADR-0014](0014-rule-based-optimizer.md), [ADR-0015](0015-security-core-and-policy-provider-contract.md), [ADR-0016](0016-security-expression-sublanguage.md), [ADR-0023](0023-leaf-operators-and-storage-pushdown.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md)
- PostgreSQL row-level security (`USING`, `WITH CHECK`), security barrier views
