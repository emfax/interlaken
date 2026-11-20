---
id: 0017
title: Entities and Hierarchy Closure Table
status: implemented  # proposal | accepted | implemented | superseded
tags: [security, catalog]
feature-flag:
created: 2026-11-17
accepted: 2026-11-19
implemented: 2026-11-19
spec-sections: ["ir-design.md#entities-and-the-closure-table"]
superseded-by:
---

# ADR-0017: Entities and Hierarchy Closure Table

## Status

Implemented

## Summary

Tables map to policy entity types, rows to entities and foreign keys to entity attributes. Hierarchy membership is stored as edges in a declared membership table, with a closure table of all transitive (descendant, ancestor) pairs maintained in the same transaction as each edge change. The principal's entity, attributes and ancestors are loaded from the transaction's snapshot at transaction start and registered in the read set.

## Context

Policy languages such as Cedar reason about entities and hierarchies (`User in Group`, `Document in Folder`). The `in` operator must be evaluable as a relational predicate at query time without recursion, and the principal's data must come from the same snapshot as the data it governs.

## Decision

- **Mapping.** Tables are entity types, rows are entities, foreign keys become entity attributes.
- **Declaration through a separate catalog mapping.** Entity types are declared in a `sys_entities` system table (alongside the catalog tables in [ADR-0007](0007-minimal-catalog-and-row-layer.md)), not as annotations on `CREATE TABLE`. Each entry maps an entity type to its table, its key columns, and its attribute mapping (columns and foreign keys to attribute names). Membership tables are declared there too. This keeps policy concerns out of table DDL, lets one table back several entity views, and gives the generated Cedar schema a single source. It is exposed through the `Catalog` trait ([ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md)).
- **Hierarchy.** Membership edges live in a declared membership table. A **closure table** of all transitive (descendant, ancestor) pairs is maintained in the same transaction as every edge change, so `in` is a single indexed semi-join at query time ([ADR-0016](0016-security-expression-sublanguage.md)).
- **Principal loading.** At transaction start, the engine loads the principal's entity, attributes and ancestors from the transaction's snapshot with `get_many`, and registers those keys in the read set via `register_read` / `register_range` ([ADR-0006](0006-batched-async-storage-trait.md), [ADR-0019](0019-transactional-authorization-and-residual-cache.md)).
- Cedar's schema is generated from these catalog entity declarations ([ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md)).

### As implemented (I4)

- **Declarations.** `EntityDef` gives an entity type its table, key column and attributes (a column, optionally a reference to another entity type). `MembershipDef` gives a member type and a group type their edges table and closure table (descendant, ancestor, path count).
  - Both are exposed through the `Catalog` trait: `entity_by_name`, `entity_for_table`, `membership` and `memberships_of`.
  - In-memory catalogs declare them in the text form (`(entity ...)`, `(membership ...)`).
  - Keys are single columns for now.
- **Closure maintenance.** `security::Closure` maintains a hierarchy by path counting: adding or removing an edge adjusts the path counts of every affected pair. Removal is therefore correct when another path still connects a pair. Cycles are rejected. A property test checks it against reachability over random edge changes.
  - *As implemented (F2):* DML on an edges table writes the closure rows in the same statement, path-counted across every hierarchy the edge connects, with cycles rejected ([ADR-0032](0032-authorization-completeness.md)).
- **Principal loading.** `interlaken_engine::load_principal` reads the principal's row and its closure rows through the transaction. The closure scan uses the principal's key prefix, so unrelated memberships do not join the read set.
  - *As implemented (F2):* `load_entities` also loads the other entities policies read (`principal.manager`), the same way ([ADR-0032](0032-authorization-completeness.md)).

## Implementation Phases

- [x] **Phase 1: Entity mapping and closure table** - I4

## Consequences

### Positive
- Hierarchy checks are a single indexed semi-join.
- Closure maintenance is transactional, so membership is always consistent with the snapshot.
- The closure table generalizes to relationship reachability for ReBAC providers.

### Negative
- Deep or wide hierarchies make edge changes expensive. Acceptable for target workloads; measure.
- Entity mappings must be kept in step with table DDL; dropping or altering a mapped column must be checked against `sys_entities`.

## Open Questions

- None.

## Future Work

- ReBAC-style relationship reachability on the closure table.

## References

- `ir-design.md` — Entities and the closure table; Risks (closure table write cost)
- [ADR-0006](0006-batched-async-storage-trait.md), [ADR-0007](0007-minimal-catalog-and-row-layer.md), [ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md), [ADR-0015](0015-security-core-and-policy-provider-contract.md), [ADR-0016](0016-security-expression-sublanguage.md), [ADR-0019](0019-transactional-authorization-and-residual-cache.md), [ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md)
