---
id: 0016
title: Security Expression Sublanguage
status: proposal  # proposal | accepted | implemented | superseded
tags: [security, ir]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["ir-design.md#security-expression-sublanguage"]
superseded-by:
---

# ADR-0016: Security Expression Sublanguage

## Status

Proposal

## Summary

Policy residuals are written in a small, decidable sublanguage with no nulls, no general recursion and no arbitrary function calls. Each operator has a defined lowering into relational IR. Errors are contained per policy with `Try(cond, false)`, and policies combine as `OR(permits) AND NOT OR(forbids)` for Cedar.

## Context

Providers need a common target for partial evaluation. If residuals could contain arbitrary SQL, they would be neither analysable nor guaranteed leakproof. A restricted, leakproof-only sublanguage lets residuals always be pushed into storage scans ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md)) and lets providers run analysis at deployment ([ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md)).

## Decision

**Types:** bool, int64 (overflow is an error), string, entity reference, record, set, and registered extension scalars (IP address, decimal, datetime). There are no nulls; SQL NULL becomes "absent" at the boundary ([ADR-0010](0010-typed-relational-ir.md)).

| Operator | Meaning | Lowers to |
| --- | --- | --- |
| Attribute path (`resource.owner.dept`) | Read an attribute, possibly through references | Column reference, or a join along a foreign key |
| `has` | Attribute is present | Presence test (not SQL NULL semantics) |
| `is` | Entity type test | Usually resolved when the plan is built |
| `in` (hierarchy) | Entity is a transitive member of another | Semi-join against the closure table ([ADR-0017](0017-entities-and-hierarchy-closure-table.md)) |
| Set contains / containsAll / containsAny | | Array operators, or a semi-join on a child table |
| `like` | Glob match with `*` | Pattern predicate |
| `==` `!=` `<` `<=` `>` `>=` | | Comparisons |
| `&&` `\|\|` `!`, if-then-else | Short-circuit | Boolean operators with ordered evaluation |
| `+` `-` `*` | Checked int64 arithmetic | Arithmetic that raises on overflow |
| Extension function | e.g. `ip.isInRange` | Registered pure, leakproof functions only |
| `Try(expr, default)` | Error containment | Per-row: an error yields `default` |

**Combining.** Each policy's condition is wrapped as `Try(cond, false)`, because Cedar ignores a policy whose evaluation errors, whether permit or forbid. The decision is `OR(permits) AND NOT OR(forbids)`. Other providers may declare a different combining rule through `PolicyProvider::combining_rule` ([ADR-0015](0015-security-core-and-policy-provider-contract.md)).

Errors raised while evaluating a residual are contained per row by `Try` and never surface; the executor implements this with an error mask ([ADR-0024](0024-expression-evaluation-with-postgres-semantics.md)).

### As implemented (I4)

- **Evaluation.**
  - **Concrete evaluation** follows Cedar: reading an absent attribute or a missing entity is an error; `==` between values of different kinds is false; integer arithmetic is checked; `&&`, `||` and `if` short-circuit left to right. It is the security oracle's ground truth.
  - **Partial evaluation** substitutes the principal (its attributes and ancestors) and the context. It simplifies only where errors cannot change the outcome: `false && x` becomes `false`, but `x || true` stays, because `x` may fail first.
- **Lowering to IR.** The residual becomes an IR expression over the resource row.
  - **IR additions.** Lowering added two things to the IR:
    - **`Try(expr, default)`**, a privileged expression that the verifier rejects from clients;
    - **the built-in `assert_present`**, which raises on NULL. Every nullable attribute is read through it, so an absent attribute fails the policy as in Cedar rather than propagating as SQL NULL, which behaves differently under `AND`/`OR`.
  - **Attribute paths through references** become scalar subqueries on the referenced entity's table. The optimizer turns these into primary-key left joins.
  - **Hierarchy `in`** becomes an `EXISTS` on the closure table, or key equalities when the principal (whose ancestors are known) is the member.
  - **Set containment** becomes `EXISTS` over `UNNEST`.
  - **`if`** becomes `(c && a) || (!c && b)`, which is equal including errors, so no subquery lands in a `CASE` branch.
  - **Globs** become `LIKE` patterns.
- **Safety rules.** The oracle found that both of these are needed:
  - **Nothing raises inside a policy subquery.** Subqueries are evaluated as applies outside the `Try`, so presence checks go outside them.
  - **Boolean simplification never removes an evaluated operand that could decide the result.** An operand that cannot raise can still decide whether a later one runs.

## Implementation Phases

- [x] **Phase 1: Sublanguage and lowering** - I4

## Consequences

### Positive
- Residuals are analysable and always leakproof, so they can always move into the scan.
- Per-policy error containment matches Cedar semantics.

### Negative
- Policies needing constructs outside the sublanguage cannot be expressed.
- Containing errors as `false` means a buggy permit silently denies, and a buggy forbid silently stops applying; deployment gates ("never errors") mitigate this.

## Open Questions

- None recorded in the source.

## Future Work

- Additional extension scalars and functions as providers need them.

## References

- `ir-design.md` — Security expression sublanguage
- [ADR-0010](0010-typed-relational-ir.md), [ADR-0015](0015-security-core-and-policy-provider-contract.md), [ADR-0017](0017-entities-and-hierarchy-closure-table.md), [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md), [ADR-0024](0024-expression-evaluation-with-postgres-semantics.md)
- Cedar policy language
