---
id: 0019
title: Transactional Authorization and Residual Cache
status: proposal  # proposal | accepted | implemented | superseded
tags: [security, transactions]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["ir-design.md#transactional-semantics", "ir-design.md#residual-cache"]
superseded-by:
---

# ADR-0019: Transactional Authorization and Residual Cache

## Status

Proposal

## Summary

Policy data read during security lowering (principal entities, closure rows, policy-version keys) joins the transaction's SSI read set, so concurrent revocations conflict with dependent SERIALIZABLE transactions. Policy sets are versioned in the catalog and changed transactionally. Authorization decisions and governed data always come from the same snapshot. Residuals are cached by a key that captures everything they depend on.

## Context

If authorization reads policy data outside the transaction, a role can be revoked while a transaction that relied on it commits, and decisions can be inconsistent with the data. Partial evaluation per execution is costly, so residuals should be cached, but the cache must never serve a stale decision.

## Decision

### Transactional semantics

- **Read-set registration.** Principal entities, closure rows and policy-version keys read during lowering are registered with `register_read` / `register_range` ([ADR-0006](0006-batched-async-storage-trait.md)). Under SERIALIZABLE, a concurrent role revocation conflicts with transactions that relied on the role ([ADR-0004](0004-optimistic-transactions-and-isolation-levels.md)).
- **Versioned policies.** Policy sets are stored in the catalog with a version and changed in transactions like DDL ([ADR-0007](0007-minimal-catalog-and-row-layer.md)). A transaction uses the policy version from its snapshot.
- **Consistency.** The authorization decision and the data it governs always come from the same snapshot.

**As implemented (I4).**
- **Principal data.** `interlaken_engine::load_principal` reads the principal's entity row and closure rows through the transaction, so they join its read set.
- **Policies.** The policy set is one row of the `policy_set` table: its text and a version. `store_policies` validates the text and bumps the version. `load_policies` reads the row and registers a dependency on it, so a writing transaction conflicts with a concurrent policy change at every isolation level.
- **Tests:**
  - a concurrent role revocation fails a SERIALIZABLE writer that relied on the role, and commits under REPEATABLE READ (authorization used the snapshot);
  - another principal's revocation does not conflict;
  - a concurrent policy change fails writers at all three isolation levels.

### Residual cache

- **Key:** (principal, action, table, policy version, version of the principal's entity data, the context fields the residual depends on).
- **Invalidation:** a policy change or a change to the principal's attributes or memberships bumps the relevant version.
- **Not cached:** residuals that depend on fast-changing context (time, IP), as reported by `PolicyProvider::dependencies` ([ADR-0015](0015-security-core-and-policy-provider-contract.md)).
- **As implemented (I5):** `security::ResidualCache` wraps any provider (`cache.provider(inner, policy_version)`). The key holds the principal, action, resource type and policy version, plus:
  - a fingerprint of the principal's attributes and ancestors, when the provider reports reading them;
  - the values of the context fields the policies read.

  Residuals reading `time` or `ip` are not cached. Every input that can change is in the key, so stale entries are never hit again, and the cache is cleared at capacity.
- **Per node.** Each node keeps its own cache. Because invalidation keys on catalog and policy versions, which Raft replicates with the data, each node invalidates correctly from its own applied state; no shared cache or cross-node invalidation protocol is needed.

## Implementation Phases

- [x] **Phase 1: Read-set registration and versioned policies** - I4
- [x] **Phase 2: Residual cache** - I5

## Consequences

### Positive
- Revocations are serializable with respect to the transactions they affect.
- Decisions are reproducible from a snapshot.
- Caching avoids repeated partial evaluation for stable principals.

### Negative
- Policy reads enlarge read sets and can raise abort rates under SERIALIZABLE.
- Cache keys and invalidation depend on correct `dependencies` reporting by each provider.
- Under weaker isolation levels, revocation races are not detected (read-set conflicts are only checked under SERIALIZABLE).

## Open Questions

- None.

## Future Work

- Measure cache hit rates across nodes once Raft exists; a shared cache is only worth it if per-node warm-up proves costly.

## References

- `ir-design.md` — Transactional semantics, Residual cache
- [ADR-0004](0004-optimistic-transactions-and-isolation-levels.md), [ADR-0006](0006-batched-async-storage-trait.md), [ADR-0007](0007-minimal-catalog-and-row-layer.md), [ADR-0015](0015-security-core-and-policy-provider-contract.md), [ADR-0017](0017-entities-and-hierarchy-closure-table.md)
