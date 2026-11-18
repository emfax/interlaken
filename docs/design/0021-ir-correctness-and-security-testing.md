---
id: 0021
title: IR Correctness and Security Testing Strategy
status: proposal  # proposal | accepted | implemented | superseded
tags: [ir, security, testing]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["ir-design.md#testing-strategy", "ir-design.md#milestones", "ir-design.md#summary"]
superseded-by:
---

# ADR-0021: IR Correctness and Security Testing Strategy

## Status

Proposal

## Summary

IR correctness is checked with golden tests, verifier fuzzing and differential testing against a naive reference interpreter. Security is checked with an oracle (policy-applied results equal unrestricted results filtered through `authorize`) and a leak suite. An RLS-style provider proves the security core before Cedar is integrated.

## Context

The IR is the single enforcement point for access control, so a bug in lowering or the optimizer is a data leak. Optimizer rewrites must preserve results. Both properties need independent oracles rather than tests that restate the implementation.

## Decision

| Technique | Checks |
| --- | --- |
| Golden tests in the text format | Resolution, lowering and optimized plans for a curated query set |
| Verifier fuzzing | Malformed and adversarial IR is rejected, never panics |
| Differential testing | Random valid IR runs optimized and unoptimized (against a naive reference interpreter); results must match |
| **Security oracle** | For random queries and principals: result with policies equals the unrestricted result filtered row by row through `authorize` |
| **Leak suite** | Queries designed to leak through errors, non-leakproof functions, join order and LIMIT; none may reveal a hidden row |
| Transactional tests | Concurrent role revocation conflicts with dependent SERIALIZABLE transactions |
| Deployment gates | Known-bad policy sets are rejected with counterexamples |

- The naive reference interpreter is shared with the executor's differential tests ([ADR-0022](0022-vectorized-arrow-stream-executor.md)); the oracle and leak suite also run end to end through the executor ([ADR-0025](0025-runtime-security-enforcement-in-executor.md)).
- An RLS-style provider is used in I4 so the oracle and leak suite pass before Cedar exists; I5 re-runs them with Cedar.
- **Phase 1 as implemented (I1):**
  - Golden files in `crates/ir/tests/golden/` must print back byte for byte (`UPDATE_GOLDEN=1` rewrites them in canonical form).
  - Each golden file has a matching query built independently with the Rust builder, and the two must be equal both as IR and as text.
  - Property tests generate random IR covering every operator, expression, type and literal, including awkward identifiers and special floats. Printing then parsing must give equal IR, and the printed text must be a fixed point.
  - Damaged and arbitrary text must never panic the reader.
- **Phase 2 as implemented (I2):**
  - Resolver golden files in `crates/ir/tests/resolve/` check names, bindings, conversions, overloads, every subquery lowering, DML and ten error cases against a fixture catalog. Each resolved plan must also verify and must resolve again to itself.
  - Targeted tests reject each class of bad plan, including privileged operators and system-table writes from clients.
  - Fuzzing has three parts:
    - random IR whose IDs hit the fixture catalog must never panic the resolver or the verifier;
    - random well-typed queries (joins, aggregates, nesting and every subquery form): whatever the resolver accepts, the verifier must accept, with at least 60% resolving so the property is not vacuous;
    - resolved plans with random subtrees swapped in must never panic the verifier and must be rejected if they contain names.
  - A planted resolver bug (dropping implicit casts) is caught by the fuzzer.

### MVP success criteria

- A hand-built IR query and the same query through the Rust builder produce identical plans and results.
- For every query in a randomized suite, the policy-applied result equals the unrestricted result filtered through `authorize`.
- No test in the leak suite (errors, UDF side effects, timing-independent probes) reveals a forbidden row.
- Correlated subqueries and GraphQL-shaped nested queries decorrelate where possible and return correct nested results.

### Milestones

Roughly 14 weeks for one engineer. I1–I3 start after storage milestone M2 and run against the in-memory `Catalog` ([ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md)). I4 needs the row layer and indexes for the closure table, so it requires storage M4.

| # | Milestone | Duration | Exit criteria |
| --- | --- | --- | --- |
| I1 | Types, expressions, operators, builder, text format | 3 weeks | Round-trip text ↔ builder; golden tests |
| I2 | Resolver, verifier, function catalog with properties | 2 weeks | Verifier fuzzing clean; privileged nodes rejected |
| I3 | Optimizer rules and decorrelation | 3 weeks | Differential tests pass; nested queries decorrelate |
| I4 | Security core: context, sublanguage, PolicyFilter, WriteCheck, barrier rules, closure table | 3 weeks | Security oracle and leak suite pass with an RLS-style provider |
| I5 | Cedar provider, residual cache, deployment gates | 3 weeks | Oracle passes with Cedar; gates reject bad policy sets |

## Implementation Phases

- [x] **Phase 1: Golden tests and builder/text round-trip** - I1
- [x] **Phase 2: Verifier fuzzing** - I2
- [ ] **Phase 3: Reference interpreter and differential testing** - I3
- [ ] **Phase 4: Security oracle, leak suite, transactional tests with RLS provider** - I4
- [ ] **Phase 5: Oracle with Cedar, deployment gate tests** - I5

## Consequences

### Positive
- Independent oracles for both result correctness and authorization correctness.
- Security core is de-risked independently of Cedar.

### Negative
- The reference interpreter and test harnesses are a significant engineering investment.
- The leak suite only covers the leak classes we think of.

## Open Questions

- None recorded in the source.

## Future Work

- sqllogictest and Postgres differential runs once the SQL front end exists.

## References

- `ir-design.md` — Summary (success criteria), Testing strategy, Milestones
- [ADR-0009](0009-storage-correctness-testing-strategy.md), [ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md), [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md), [ADR-0022](0022-vectorized-arrow-stream-executor.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md)
