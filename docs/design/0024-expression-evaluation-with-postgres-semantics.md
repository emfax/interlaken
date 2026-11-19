---
id: 0024
title: Expression Evaluation with Postgres Semantics
status: accepted  # proposal | accepted | implemented | superseded
tags: [executor, expressions, postgres-compat]
feature-flag:
created: 2026-11-17
accepted: 2026-11-19
implemented:
spec-sections: ["executor-design.md#expression-evaluation", "executor-design.md#testing-strategy"]
superseded-by:
---

# ADR-0024: Expression Evaluation with Postgres Semantics

## Status

Accepted

## Summary

Expressions are interpreted vectorized over Arrow arrays, reusing `arrow-rs` kernels only where their semantics match Postgres and using our own kernels otherwise. Evaluation follows three-valued logic and Postgres error semantics, uses selection vectors so short-circuit constructs never raise errors from branches not taken, and evaluates the security sublanguage's `Try` with per-row error masks.

## Context

The IR fixes Postgres semantics: overflow and division by zero are errors, filters keep rows only when TRUE, and `CASE`/`COALESCE`/`NULLIF` must not raise errors from untaken branches ([ADR-0010](0010-typed-relational-ir.md)). Naive vectorized evaluation computes every branch on every row, so `CASE WHEN x <> 0 THEN 10 / x END` would raise division by zero where Postgres doesn't. `arrow-rs` kernels wrap or produce NULL in places Postgres raises, and differ on some NULL and cast behaviour. The security sublanguage requires per-row error containment ([ADR-0016](0016-security-expression-sublanguage.md)). JIT or WASM code generation would be faster but is out of MVP scope.

## Decision

- **Vectorized interpretation** over Arrow arrays. An `arrow-rs` kernel is used only once semantic tests show it matches Postgres; otherwise we implement our own.
- **Three-valued logic.** Filters keep rows only where the predicate is TRUE; NULL and FALSE both drop the row. `Filter` produces a selection vector.
- **Postgres error semantics.** Integer overflow, division by zero and failed casts raise errors with the matching SQLSTATE rather than wrapping or producing NULL.
- **Selection vectors for short-circuit evaluation.** `CASE`, `AND`, `OR` and `COALESCE` evaluate each branch only on the rows that reach it.
- **Error containment for `Try(expr, default)`.** Evaluated with an error mask: rows whose evaluation fails get `default` instead of failing the batch.

**Testing.**

- Kernel property tests: NULL propagation, three-valued logic, overflow and division errors, cast rules.
- Short-circuit tests: expressions whose untaken branches would error must not raise.

## Implementation Phases

- [x] **Phase 1: Basic vectorized Filter and Project expressions** - E1
  - *As implemented:* E1 evaluated expressions row at a time with the reference interpreter's `RowEvaluator`; E2 replaced this.
- [x] **Phase 2: Expression kernels with Postgres semantics and selection vectors** - E2
  - *As implemented:* `interlaken_exec::CompiledExpr` compiles an expression once per operator: column positions are resolved, parameters bound, constant `LIKE` patterns compiled, and each node's type (from the new `verify::expr_type`) and referenced columns recorded. Kernels are our own; no `arrow-rs` compute kernel is reused, because each one we looked at differs from Postgres on overflow, float ordering (-0 and NaN) or modulo. `arrow-select` provides `take`, `filter`, `interleave`, `nullif` and `concat`, which only move values.
  - *Selection:* instead of selection vectors carried through every kernel, short-circuit forms compact the rows that reach an operand with `take` (only the columns that operand reads) and merge the results back by row with `interleave`. Each row evaluates exactly the subexpressions it does in the interpreter, so a batch fails if and only if some row fails, with an error that row raises.
  - *Shared semantics:* casts to and from text call the interpreter's public `cast_value`, and `LIKE` uses its public `LikePattern`, so there is one definition of parsing, formatting and matching.
  - *Testing:* a differential test evaluates 4,000 random typed expressions (every scalar type and operator, guarded divisions, `CASE`, `COALESCE`, `NULLIF`, `Try`, casts) over random batches with the kernels and with the interpreter row by row, and checks single rows for identical results and errors. Planted bugs in `AND` and `CASE` short-circuiting, integer overflow, float ordering, `COALESCE` merging, `Try`, `LIKE` and date overflow are each caught.
- [x] **Phase 3: `Try` error masks for policy residuals** - E2 (brought forward from E5)
  - *As implemented:* a failing batch is bisected until the failing rows are isolated; only those get the default. Cost is about k·log n evaluations for k failing rows, and nothing for batches that do not fail.

## Consequences

### Positive
- Postgres-compatible results and errors, a prerequisite for the SQL front end.
- Short-circuit correctness without giving up vectorization.
- Contained residual errors support the no-leak guarantee ([ADR-0025](0025-runtime-security-enforcement-in-executor.md)).

### Negative
- Every reused `arrow-rs` kernel needs semantic tests; mismatches mean writing and maintaining our own kernels.
- Selection-vector evaluation is more complex than whole-array kernels and slower on branch-heavy expressions.

## Open Questions

- None from the source proposal.

## Future Work

- JIT or WASM code generation for expressions.
- Expanding the set of built-ins proven leakproof ([ADR-0011](0011-function-property-catalog.md)).

## References

- `executor-design.md` — Expression evaluation, Testing strategy
- [ADR-0010](0010-typed-relational-ir.md), [ADR-0011](0011-function-property-catalog.md), [ADR-0016](0016-security-expression-sublanguage.md), [ADR-0022](0022-vectorized-arrow-stream-executor.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md)
- PostgreSQL error codes (SQLSTATE) documentation
