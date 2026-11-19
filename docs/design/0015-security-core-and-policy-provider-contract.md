---
id: 0015
title: Security Core and Policy Provider Contract
status: implemented  # proposal | accepted | implemented | superseded
tags: [security, ir]
feature-flag:
created: 2026-11-17
accepted: 2026-11-19
implemented: 2026-11-19
spec-sections: ["ir-design.md#security-core", "ir-design.md#principles", "ir-design.md#security-context", "ir-design.md#provider-contract", "ir-design.md#other-providers", "ir-design.md#hand-built-ir-and-wasm"]
superseded-by:
---

# ADR-0015: Security Core and Policy Provider Contract

## Status

Implemented

## Summary

Access control is enforced in one place: security lowering on the IR. The IR defines a small security core; policy engines plug in as providers that partially evaluate policies into residual predicates. Cedar is the first provider. Each transaction binds a typed `SecurityContext` that the engine supplies, never the client.

## Context

The engine accepts queries from SQL, GraphQL, hand-built IR and WASM host calls. Enforcing access control in each front end risks gaps and inconsistent semantics. Tying the engine to one policy language limits adoption. Authorization must also be consistent with the data it governs, which requires it to be transactional.

## Decision

### Principles

1. **One enforcement point.** Every read and write from SQL, GraphQL, hand-built IR and WASM host calls passes through security lowering on the IR.
2. **Policy engines are providers.** The IR defines a small security core; Cedar is the first provider that compiles into it.
3. **Default deny.** No applicable permit means no access.
4. **Authorization is transactional.** Policy data is read from the transaction's snapshot and joins its read set ([ADR-0019](0019-transactional-authorization-and-residual-cache.md)).
5. **Policies are analysed before they run.** Providers that support analysis gate policy deployment ([ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md)).

### Security context

Each transaction binds a typed `SecurityContext`:

- **Principal:** an entity reference from the authenticated identity (verified JWT/OIDC token mapped to a principal entity).
- **Action:** set per operation by lowering (for example, `read` on table X).
- **Context:** a record of request facts with two parts:
  - **Standard fields**, defined and filled by the engine: request time, source IP, authentication method/strength, session ID. Policies using only these are portable across applications.
  - **Application record**, a typed record whose fields are declared by the application in the Cedar schema ([ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md)). The engine type-checks it against the schema but never fills or interprets it.

The context is bound by the engine, never supplied in client IR; the verifier rejects overrides ([ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md)).

### Provider contract

```rust
pub trait PolicyProvider: Send + Sync {
    /// Principal, action and context known; resource symbolic.
    fn partial_eval(&self, req: &PartialRequest, entities: &EntitySlice)
        -> Result<Residual>;              // Allow | Deny | Conditional(SecExpr)

    /// Fully concrete request, used for writes.
    fn authorize(&self, req: &Request, entities: &EntitySlice) -> Result<Decision>;

    /// What a residual depends on, for caching and invalidation.
    fn dependencies(&self, residual: &Residual) -> Dependencies;

    /// How individual policy results combine.
    fn combining_rule(&self) -> CombiningRule;   // Cedar: permit-unless-forbid, default deny
}
```

Residuals are expressed in the security sublanguage ([ADR-0016](0016-security-expression-sublanguage.md)) and lowered into `PolicyFilter` / `WriteCheck` ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md)). Analysis (equivalence, never-errors, always-allows) stays inside the provider and runs at deployment time, outside the query path.

### Other providers

| Provider | Fit |
| --- | --- |
| Postgres-style RLS | Policies are already predicates: the trivial provider |
| OPA/Rego | Partial evaluation yields residual conditions of the same shape |
| Relationship-based (OpenFGA, SpiceDB style) | Hierarchy membership generalizes to relationship reachability through the closure table |
| Plain RBAC | Membership plus constants: a subset of the core |

An RLS-style provider is built first (I4) to prove the core independently of Cedar.

### As implemented (I4)

The security core lives in `interlaken_ir::security`.
- **`SecurityContext`** holds the principal and the standard fields (`time`, `ip`, `auth_method`, `session_id`). The application record is visible to policies as `context.app`.
- **`PolicyProvider`** follows the contract above, with three adjustments:
  - `authorize` takes the resource as data (`ResourceData`), so rows not yet written can be checked;
  - `dependencies` takes the partial request, because the residual no longer mentions the context it consumed;
  - a `column_access` method decides column-level reads ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md)).
- **`RlsProvider`** holds sublanguage policies, written in the text form `(permit (read update) Doc COND)`. Its `partial_eval` partially evaluates each applicable policy and combines the results as `OR(permits) AND NOT OR(forbids)`. A table with no applicable permit is denied.
- **`secure(plan, catalog, provider, context, entities)`** is security lowering ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md)). It fails closed: if a policy cannot be lowered, the query does not run.

### Hand-built IR and WASM

- Hand-built IR and WASM query builders enter through the same resolve, verify and lower pipeline. The privileged-node check stops clients injecting a fake `PolicyFilter` or claiming a function is leakproof.
- WASM host calls never get raw storage access. Their queries carry the caller's security context, bound by the engine.
- Physical plans supplied by clients (later, admin-only) still have the policy residual applied: its single-row part inside the storage `ScanRequest`, and any join-dependent part as an engine-inserted `PolicyFilter` ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md)).

## Implementation Phases

- [x] **Phase 1: Security context, provider contract, RLS-style provider** - I4
- [x] **Phase 2: Cedar provider** - I5
  - *As implemented:* `interlaken_ir::security::cedar` translates Cedar policies to the security sublanguage against a schema generated from the catalog, and serves them through the same provider contract as the RLS-style provider ([ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md)). The security oracle runs with it (`security_cedar.rs`).

## Consequences

### Positive
- No entry point can bypass authorization.
- Pluggable providers allow Cedar, RLS, OPA, ReBAC or RBAC without changing the core.
- Authorization is consistent with the data snapshot.

### Negative
- Every provider must support partial evaluation into the sublanguage.
- Security lowering runs per execution, adding planning cost.

## Open Questions

- None.

## Future Work

- Additional providers (OPA/Rego, ReBAC, RBAC).
- Admin-only client-supplied physical plans.

## References

- `ir-design.md` — Security core: Principles, Security context, Provider contract, Other providers, Hand-built IR and WASM
- [ADR-0006](0006-batched-async-storage-trait.md), [ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md), [ADR-0016](0016-security-expression-sublanguage.md), [ADR-0017](0017-entities-and-hierarchy-closure-table.md), [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0019](0019-transactional-authorization-and-residual-cache.md), [ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md)
- Cedar, OPA/Rego, OpenFGA, SpiceDB, PostgreSQL RLS
