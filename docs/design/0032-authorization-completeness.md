---
id: 0032
title: Authorization Completeness - Closure Maintenance, Entity Loading, Column Masks and Impact at Scale
status: implemented  # proposal | accepted | implemented | superseded
tags: [security, executor, engine]
feature-flag:
created: 2026-11-20
accepted: 2026-11-20
implemented: 2026-11-20
spec-sections: ["mvp-completion.md#f2-authorization-completeness"]
superseded-by:
---

# ADR-0032: Authorization Completeness

## Status

Implemented

## Summary

Four gaps left authorization incomplete after F1. This ADR closes them:
- **Closure maintenance in DML.** The physical planner attaches a closure-maintenance step to every DML statement on a membership edges table. It keeps the path-counted closure tables exact in the same statement, so authorization later in the transaction sees the new memberships.
- **Entities beyond the principal.** Providers list the entities their policies read through the principal or by name (`principal.manager.dept`). The engine loads them through the transaction before partial evaluation, and their reads join the read set like the principal's.
- **Column masks.** A column policy may declare a mask. Where the column's policies depend on the row, rows they deny show the mask (NULL or a masking function of the value) instead of failing the query. `EXPLAIN` hides masks.
- **Impact analysis at scale.** Impact analysis prunes by the changed policies' actions and resource types, decides incrementally per changed policy, and samples above a threshold, saying when it did.

## Context

`mvp-completion.md` (F2) lists these as the remaining authorization work:
- `closure-maintenance-in-dml`. E4 made DML on edges tables possible, but nothing wrote the closure rows. `security::Closure` (ADR-0017) maintained a closure only in memory, so DML left the closure stale and authorization wrong.
- `entity-loading-beyond-principal`. `load_principal` loaded only the principal. A policy reading `principal.manager.dept` failed, and was then ignored (Cedar's rule).
- `column-masking`. A column policy that depends on the row denied the column outright (ADR-0018, fail closed).
- `impact-analysis-at-scale`. Impact analysis authorized every principal against every row, for every action (ADR-0030): quadratic in the data.

## Decision

### Closure maintenance in DML

**Plan time.** `ClosurePlan::for_table` (in `interlaken-exec`'s new `closure` module) finds the memberships whose edges table is the DML target. It also finds every closure table an edge change can touch. Hierarchies can span types (documents in folders in folders), so an edge `m -> g` of membership `(M, G)` can connect any type `T` that reaches `M` to any type `A` reachable from `G`. The planner requires these memberships to be declared:
- `(T, M)` and `(G, A)`, to find the entities on either side;
- `(T, A)`, to store their pairs;
- `(G, M)`, if declared, for cycle detection.

That is, the declared memberships must be closed under composition for the types involved; otherwise the statement fails with `0A000`. A closure table's primary key must be its descendant and ancestor columns.

The planner records the plan on the DML operator (`Dml::closure`). `EXPLAIN` shows it as `maintaining closures: ...`. DML that targets a closure table directly is refused (`42501`): only the engine writes those.

**Run time.** As each batch is written, `Maintainer` records every edge change: an insert adds an edge, a delete removes one, an update removes the old edge and adds the new. Then it applies them in statement order with path counting, as `security::Closure` does:
- Adding `m -> g` adds `paths(x, m) * paths(g, y)` to `(x, y)` for every `x` reaching `m` (including `m`) and every `y` reachable from `g` (including `g`). Removing subtracts the same, and a pair whose count reaches zero is deleted.
- An added edge that closes a cycle fails the statement with `23514`.
- A removal that would make a count negative means the closure table was already inconsistent, and fails with `XX000`.

Storage reads hide the current statement's writes (ADR-0026), so the maintainer keeps the counts it has read or changed in memory, charged to the query's memory budget. It writes each changed pair once per batch: an insert, update or delete of the closure row.

**Lookups.** A lookup by descendant or ancestor uses a primary-key prefix scan where the key starts with that column. Otherwise it uses an index whose first column it is, or else a full scan.

**Concurrency.** Two transactions that each add one edge of a chain (`3 -> 2` and `2 -> 1`) don't see each other's edge, so neither would add `(3, 1)`. Under snapshot isolation they write different keys and both would commit, leaving the closure wrong. So every closure range and row the maintainer reads is registered as a dependency (`rows::register_scan_dependency`, `register_row_dependency`). The later committer then fails with `40001` at every isolation level, and a retry sees the first edge. The reads are also ordinary reads, so under SERIALIZABLE a membership change conflicts with transactions whose authorization relied on the old closure, as in ADR-0019.

**Same-transaction authorization.** Each statement loads the principal (and other entities) through the transaction, which sees earlier statements' writes. The residual cache keys on the principal's memberships (ADR-0019), so the next statement's authorization uses the new closure.

### Entities beyond the principal

An `EntityPath` names an entity a policy reads that isn't the resource. It is a root (the principal or an entity literal) followed through attributes that hold entity references:
- `principal.manager.dept` reads the entity at `principal.manager`;
- `principal.manager in Group::"1"` reads its ancestors;
- `User::"7".dept` reads the literal entity.

`collect_entity_paths` finds them in a sublanguage expression. Each provider lists the paths of all its policies through `PolicyProvider::entity_paths`:
- `RlsProvider` walks its policies.
- `CedarProvider` translates its policies' conditions with a placeholder principal and walks those.
- `CachedProvider` passes the call through.

The statement pipeline now builds the provider before loading entity data. `load_entities` then loads the principal and follows each path through the transaction. Each entity on the way is loaded like the principal: its row with `get`, and its closure rows by key prefix. All of them join the read set, so under SERIALIZABLE a concurrent change to the manager's department fails a writer that relied on it. A path stops where an attribute is absent or not an entity. An entity that doesn't exist is recorded as missing, so reading its attributes fails as Cedar specifies.

Partial evaluation then knows those entities and substitutes them. Cedar's TPE also receives them as known entities. Resource-rooted paths (`resource.owner.dept`) are still lowered to joins.

**Caching.** The residual cache key gains a fingerprint of every path's entity: its reference, attributes and ancestors. A change to the manager's department then misses the cache, as a change to the principal's own data does.

### Column masks

**Declaring masks.** A column policy may declare a mask:
- in RLS, `:mask null` or `:mask FUNCTION` after `:column`;
- in Cedar, the annotation `@mask("...")` beside `@column`.

A mask is NULL or a scalar function of the column's value that returns the column's type. Two built-ins are added: `redact` (always `****`) and `mask_last4` (every character but the last four replaced by `*`). Any catalog function with the right signature works.

**Deciding access.** `PolicyProvider::column_mask` returns a `ColumnAccess` decision:
- `Allow` when the column's policies allow every row;
- `Deny` when they don't and declare no mask (the old behaviour);
- `Masked { visible, mask }` otherwise, where `visible` is the residual over the resource, or `false` when no row may see the column.

Conflicting masks on one column are an error. The default implementation maps `column_access`, so other providers keep failing closed.

**Lowering.** `secure` collects masked columns that the plan reads, per `Get`. It wraps each such `Get` (above its policy filter) in a projection that keeps the `Get`'s binding:
- every column passes through, except masked ones;
- a masked column becomes `policy_mask(visible, value, mask(value))`, named like the column.

The resolver maps references to that binding onto the projection, so everything above reads masked values: filters, joins, aggregates, the output. The mask's condition is lowered like a residual, with the same subqueries (marked `sec$`) and the same `Try` containment.

**`policy_mask`.** It is a built-in, overloaded per scalar type, and `value` where `visible` is true, else `masked`. It is volatile, so the optimizer never inlines the projection's expressions into predicates or moves them. The executor compiles it to a `CASE`, so the mask function runs only on masked rows.

**Writes.** Masks apply only to reads. A statement that writes a table whose masked column it touches fails with `42501`, as for a denied column. DML carries the whole target row, so in practice any write to that table fails for that principal. Masks on array columns are unsupported (`42501`).

**`EXPLAIN`.** For principals without `read-policies`, each mask in a projection renders as `<policy> mask`. The projection's input is treated like a write check's: subplans the mask's condition added are collapsed, and the conditions joining them are hidden. `policy_mask`'s function IDs are fixed by the built-ins, so `EXPLAIN` recognizes it without the catalog.

### Impact analysis at scale

`impact_analysis_with` takes `ImpactOptions`, with `max_decisions` (default one million) and a sampling `seed`. `impact_analysis` uses the defaults.
- **Pruning by resource type.** Providers that hold sublanguage policies expose them through `PolicyProvider::policies`. When both sets do, only the changed policies matter: those in exactly one set, counted with multiplicity. Row decisions can change only for the changed row policies' actions and resource types, and column decisions only for the changed column policies' columns. Every other decision is counted in `pruned`, unevaluated.
- **Incremental evaluation.** Under permit-unless-forbid, a decision is a function of which applicable policies hold. Unchanged policies hold or don't identically before and after. So if no changed policy that applies to a request holds, the decision is unchanged, and only the changed policies are evaluated for it. Otherwise both providers decide it (`evaluated`).
- **Sampling.** If the decisions in scope (`population`) exceed `max_decisions`, principals and resources are each kept with probability `sqrt(max / population)`. Each is chosen by a seeded hash of its reference, so about `max_decisions` decisions are compared and the same seed picks the same sample. The report sets `sampled`, and its summary says that changes outside the sample aren't shown.

Opaque providers (Cedar) expose no policies, so they are analysed in full and sampled above the threshold. The engine's local verification gate uses the defaults, and the gate's detail is the summary.

## Implementation Phases

- [x] **Phase 1: Closure maintenance in DML, entity loading beyond the principal, column masks, impact analysis at scale** - F2
  - *As implemented:*
    - Closure maintenance is in `crates/exec/src/closure.rs`. The planner's and DML operator's changes are a field and a few calls.
    - Entity paths are in `crates/ir/src/security/paths.rs`, and loading is `interlaken_engine::load_entities`.
    - Masks are in `security::plan`, with the providers' `column_mask`, and the `redact`, `mask_last4` and `policy_mask` built-ins in the interpreter and the kernels.
    - Impact analysis is in `security::impact`.
  - *Testing:*
    - **Closure oracle:** `session_membership_oracle.rs` runs, through `Session`, random DML on the three edges tables interleaved with queries in one transaction. After every statement, the closure tables must equal a model that recomputes path counts from scratch, and every query must match the interpreter over the authorized rows under the model's memberships.
    - **Closure behaviour:** `closure_maintenance.rs` covers multi-type hierarchies, a second path, an update, cycles, refused closure-table writes and same-transaction authorization with rollback.
    - **Conflicts:** a DML revocation fails a SERIALIZABLE writer that relied on it, and only under SERIALIZABLE. Concurrent edges of one chain conflict at every isolation level. A concurrent change to the manager's department fails a SERIALIZABLE writer whose policy read it.
    - **Entity paths:** the shared generators gain atoms over `principal.manager`, so the IR and session security oracles cover them. Cedar's paths and masks have a test that loads only what the paths name, and the residual cache misses when the manager changes.
    - **Masks:** `session_masks.rs` is a differential: sessions must equal the interpreter over the authorized rows, with each masked column replaced row by row wherever the column policies, decided concretely, deny. It also covers `EXPLAIN`, writes and conflicting masks. A golden file shows the lowered masking projection.
    - **Impact analysis:** pruned, incremental analysis must report exactly what a full analysis through opaque providers reports, and save work. Sampling flags itself, is reproducible by seed, and reports only real changes.

## Consequences

### Positive
- Memberships can change through ordinary DML, in transactions, and authorization stays exact, including across nested multi-type hierarchies.
- Policies can use the organisation graph (`principal.manager`), with the same snapshot and conflict guarantees as the principal's own data.
- Row-dependent column policies are usable: principals see masked values instead of failing queries.
- Policy changes on large datasets can be verified in bounded time, and the report says when it sampled.

### Negative
- Closure maintenance reads every pair above and below a changed edge, and writes their product. Deep or wide hierarchies make edge changes expensive (ADR-0017's accepted risk). A closure table without an index on its ancestor column needs a full scan to find an edge's descendants, and registers the whole table as a dependency.
  - *Measured* (`closure_maintenance_cost`, release build, buffered durability): a 200-folder chain inserted in one statement (19,900 pairs) took 0.5 s. Filing 1,000 documents in its deepest folder (199,000 pairs) took 6.2 s, about 30 µs per pair written. One edge above the whole chain (1,200 new pairs, found by full scans of 219,000 closure rows) took 0.2 s. That is acceptable for membership changes, so maintenance is neither capped nor batched; an index on the ancestor column avoids the full scans.
- Concurrent membership changes in the same hierarchy region conflict at every isolation level, and one must retry. Membership changes are rare in the target workloads.
- Declared memberships must be closed under composition for hierarchies spanning types.
- Entity paths are collected from all policies, so every statement loads every referenced entity, even for tables it doesn't touch.
- A principal can't write a table with a masked column, even without reading it. This is conservative: DML carries the whole target row.
- Sampled impact analysis may miss changes. The report says so, but the gate passes on what it saw.

## Open Questions

- Should writes be allowed when the masked column is only carried as part of the target row and never read or returned? That needs check-columns to tell carried columns from read ones (backlog: `masked-table-writes`).

## Future Work

- Batching closure maintenance for bulk edge loads (one pass over all changed edges instead of one per edge).
- Paths rooted at context values (`context.app.owner.dept`).
- Pruning for Cedar, from its policies' scopes.

## References

- [ADR-0017](0017-entities-and-hierarchy-closure-table.md), [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0019](0019-transactional-authorization-and-residual-cache.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md), [ADR-0026](0026-statement-atomicity-and-halloween-protection.md), [ADR-0028](0028-physical-operators-planning-and-explain.md), [ADR-0030](0030-policy-proposals-and-distributed-verification.md), [ADR-0031](0031-engine-sessions-and-storage-backed-catalog.md)
- `mvp-completion.md` — F2
