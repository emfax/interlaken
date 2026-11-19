---
id: 0020
title: Cedar Policy Provider and Deployment Gates
status: implemented  # proposal | accepted | implemented | superseded
tags: [security, cedar]
feature-flag:
created: 2026-11-17
accepted: 2026-11-19
implemented: 2026-11-19
spec-sections: ["ir-design.md#cedar-provider", "ir-design.md#policy-deployment-and-analysis"]
superseded-by:
---

# ADR-0020: Cedar Policy Provider and Deployment Gates

## Status

Implemented

## Summary

Cedar is the first `PolicyProvider`. Reads use Cedar's type-aware partial evaluation (TPE), translated into the security sublanguage; writes use Cedar's standard authorizer. The Cedar schema is generated from the catalog. Policy changes are transactions that must pass validation and SMT-based analysis gates (`cedar-policy-symcc` with cvc5) before committing.

## Context

Cedar offers a well-specified, analysable policy language with partial evaluation, matching the provider contract ([ADR-0015](0015-security-core-and-policy-provider-contract.md)). Its TPE API is experimental, so its churn must be contained. Bad policies (errors, accidental broadening) should be caught before deployment, not at query time.

## Decision

### Provider

- **Partial evaluation:** Cedar TPE produces a residual over the resource. A translator converts Cedar residuals into the security sublanguage ([ADR-0016](0016-security-expression-sublanguage.md)).
- **TPE is experimental.** Pin the `cedar-policy` version and keep all Cedar-specific code behind the translator, so API changes stay in one module.
- **Writes:** Cedar's standard authorizer on concrete entities built from the row.
- **Schema:** generated from the catalog's entity declarations ([ADR-0017](0017-entities-and-hierarchy-closure-table.md)), so policies are validated against real tables and attributes.
- **Combining:** permit-unless-forbid, default deny.

### Deployment gates

A policy change is a transaction that runs these gates before committing:

| Gate | Fails when |
| --- | --- |
| Validation | A policy doesn't type-check against the generated schema |
| Never errors | A policy can raise an error on some well-formed request |
| No broadening | The new set allows a request the old set denied, unless explicitly confirmed |
| Declared invariants | A stated invariant fails, such as "only owners can delete documents" |

Failures return a concrete counterexample request.

**Revised (I5): where the gates run.** The SMT gates (never errors, no broadening, declared invariants) run only on prover nodes configured with cvc5. Policy changes are proposals verified by whichever nodes can verify them ([ADR-0030](0030-policy-proposals-and-distributed-verification.md)). Every node runs validation, a forbid lint and impact analysis on its data. Embedded databases never need a solver.

### As implemented (I5)

- **Provider.** `security::CedarProvider` builds the Cedar schema from the catalog's entity declarations (`cedar_schema`):
  - nullable columns become optional attributes;
  - references are typed by the referenced entity;
  - memberships become parent types;
  - the actions `read`, `insert`, `update` and `delete` apply to every entity type, with the standard context fields and an application record type.

  Policies are validated in strict mode when the provider is constructed.
- **Partial evaluation** uses TPE with the principal's entity (attributes and ancestors) as the only known entity, so attribute paths through references stay residual for lowering. The translator converts residual policies' syntax trees into the sublanguage. Extension types, tags and unknowns are rejected, so lowering fails closed. Our partial evaluator then substitutes the principal and context, and the result is combined and lowered exactly as for the RLS provider. All Cedar-specific code is in `security::cedar`, and `cedar-policy` is pinned to `=4.15.0`.
- **Writes** use Cedar's own authorizer (`is_authorized_batched`), loading entities on demand from the entity store.
- **Column-level policies** carry a `@column("name")` annotation and take no part in row decisions.
- **Gates.** The SMT gates are implemented by the engine's `SmtProver` (feature `smt-prover`), with `cedar-policy-symcc` `=0.9.0` and cvc5 1.3.1. Invariants are Cedar policy sets describing the most the new set may allow.
- **Testing:**
  - The Cedar oracle renders random policies in Cedar. For every governed row, our sublanguage evaluator must decide exactly as Cedar's authorizer. Every secured query must return what the query returns over the rows Cedar authorizes.
  - Runs: 1,500 cases per run, and 10,000 once.
  - It found one semantic difference, which is fixed: Cedar's `has` on a missing entity is false, not an error.

## Implementation Phases

- [x] **Phase 1: Cedar provider and translator** - I5
- [x] **Phase 2: Deployment gates** - I5 (SMT gates on prover nodes, [ADR-0030](0030-policy-proposals-and-distributed-verification.md))

## Consequences

### Positive
- Policies are type-checked against real tables and analysed before they run.
- Counterexamples make gate failures actionable.
- Cedar churn is isolated to the translator.

### Negative
- Dependency on an experimental TPE API; mitigated by pinning and the translator boundary, and by building the RLS-style provider first (I4).
- SMT solver (cvc5) becomes a deployment-time dependency.

## Open Questions

- None recorded in the source.

## Future Work

- Tracking Cedar TPE as it stabilizes.

## References

- `ir-design.md` — Cedar provider, Policy deployment and analysis; Risks (Cedar TPE stability)
- [ADR-0015](0015-security-core-and-policy-provider-contract.md), [ADR-0016](0016-security-expression-sublanguage.md), [ADR-0017](0017-entities-and-hierarchy-closure-table.md), [ADR-0019](0019-transactional-authorization-and-residual-cache.md), [ADR-0021](0021-ir-correctness-and-security-testing.md)
- `cedar-policy`, `cedar-policy-symcc`, cvc5
