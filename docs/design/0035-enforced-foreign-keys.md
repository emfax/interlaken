---
id: 0035
title: Enforced Foreign Keys
status: accepted  # proposal | accepted | implemented | superseded
tags: [engine, exec, storage, security, ddl]
feature-flag:
created: 2026-11-21
accepted: 2026-11-21
implemented:
spec-sections: ["mvp-completion.md#beyond-the-mvp"]
superseded-by:
---

# ADR-0035: Enforced Foreign Keys

## Status

Accepted. Designed, not built.

## Summary

Add foreign keys that the engine enforces. A foreign key says that the values of some columns of a table (the *referencing* table) are a key of another table, or of the same one (the *referenced* table). These checks hold at the end of every statement:
- every new or changed referencing row refers to an existing referenced row;
- no deleted or re-keyed referenced row is still referred to.

Deletes can also cascade to the referencing rows or set their columns to NULL.

Enforcement lives in the DML operator, next to closure maintenance (ADR-0032). The checks read the latest state, as unique constraints do, and register their reads as dependencies. So a concurrent "insert a child" and "delete its parent" can't both commit, at any isolation level. Cascaded writes are ordinary DML on the referencing table: they pass the principal's write checks, maintain closures, and check their own foreign keys.

## Context

### What exists today

- **Constraints:** primary keys, unique indexes and NOT NULL (`reference/limits.md`). The row layer enforces them (ADR-0007), with reads that see the current statement's writes. Concurrent duplicates write the same key, so the later committer fails validation at every isolation level.
- **`references` on entity attributes** (ADR-0017, ADR-0032). An attribute declared with `references` tells the security layer that its column holds an entity of that type, so policies can compare it with `principal` or follow it to the entity's attributes. Nothing checks that the entity exists. A dangling reference makes that entity *missing*, so a policy reading its attributes fails, as Cedar specifies.
- **Membership edges** refer to member and group entities by key. Nothing stops an edge from naming an entity that doesn't exist, or an entity from being deleted while edges still name it. The closure then keeps pairs for an entity that's gone.

### Why applications can't do it themselves

An application can check that a parent exists before inserting a child, and that no children exist before deleting a parent. Under snapshot isolation those checks race:
- T1 reads parent P and inserts a child of P;
- T2, concurrently, finds no children of P and deletes P;
- they write different keys, so both commit, leaving a dangling child.

SERIALIZABLE catches this only if both transactions read exactly the right ranges. Read committed and repeatable read don't catch it at all. The engine can make the check hold at every isolation level, as it does for unique constraints and closures. An application can't.

### Why now

- The NULL discussion that led to ADR-0034 needed foreign keys for decomposed schemas. Interlaken recommends that style of schema design even with option types.
- The GraphQL front end (ADR-0012) needs declared relationships between tables for nested fields, and foreign keys are the standard declaration.
- The SQL front end will meet `REFERENCES` in DDL.
- The optimizer can use foreign keys for join elimination and cardinality estimates.

## Decision

### Declaring a foreign key

`ForeignKeySpec`, created by `create_foreign_key` and removed by `drop_foreign_key`, both engine DDL (ADR-0031):

| Field | Meaning |
| --- | --- |
| `name` | unique across the database, like index names |
| `table`, `columns` | the referencing table and columns, in order |
| `references`, `referenced_columns` | the referenced table and columns, in the same order |
| `on_delete` | `NoAction` (default), `Cascade` or `SetNull` |
| `match_full` | `false` (default, SQL's `MATCH SIMPLE`) or `true` (`MATCH FULL`) |

Each column must be part of exactly one key, and DDL rejects a foreign key with `42P16` unless all of the following hold:
- **The referenced columns are a key.** They must be the referenced table's primary key, or exactly the columns of one of its unique indexes.
- **The types match.** Each referencing column has the same type as its referenced column, ignoring nullability, as `add_membership` checks membership columns.
- **The referencing columns are indexed.** Some index on the referencing table must have the referencing columns as its leading columns, in any order. Deletes find referencing rows through it, and register its range as a dependency. Without it, every parent delete would scan the whole referencing table and depend on all of it. The error names the index to create.
- **`SetNull` columns are nullable.**
- **Neither table is an engine table** (`engine_*` or `sys_*`).

A table may refer to itself, as a folder tree does.

**Creating a foreign key checks existing rows.** It fails with `23503` at the first referencing row with no referenced row. DDL is administrators only, so the message may name that row's key.

**Storage.** Two new engine catalog tables hold the definitions:
- `engine_foreign_keys`: name, table, referenced table, referenced unique index (NULL for the primary key), `on_delete` and `match_full`;
- `engine_foreign_key_columns`: name, position, column and referenced column.

Both are written in the DDL transaction and bump the schema version, as all DDL does.

**Guards on other DDL:**
- `drop_table` refuses a table that another table's foreign key refers to. A self-referencing table's foreign keys are dropped with it.
- `drop_index` refuses an index that a foreign key relies on, on either side.

**IR catalog.** `TableDef` gains `foreign_keys: Vec<ForeignKeyDef>`. The catalog text format gains `(foreign-key name (columns ...) (references table columns ...) :on-delete ... :match ...)`. The resolver and verifier check that the definitions are consistent, as they check entity declarations.

**Relation to `references`.** The two stay separate:
- `references` is type information for policies.
- A foreign key is a constraint on data.

When both cover a column, they must agree: the foreign key must point at the referenced entity type's table and key column. Catalog load checks this (`42P16`). The book recommends declaring both. A `references` attribute without a foreign key keeps its current meaning and may dangle.

### What is checked

- **New referencing values.** For each row inserted, or updated in its referencing columns:
  - under `MATCH SIMPLE`, a row with any NULL referencing column isn't checked;
  - under `MATCH FULL`, all NULL isn't checked, and partly NULL is a violation;
  - otherwise a referenced row with those values must exist.
- **Removed referenced values.** For each referenced row deleted, or updated in its referenced columns, no referencing row may still hold the old values once the statement's cascades have run.
- **Other updates aren't checked.** An update that changes neither the referencing nor the referenced columns needs no check.

**Checks are per statement, not per row.** A constraint must hold when each statement ends, which is SQL's `NO ACTION`. So one statement can do either of these in any order:
- insert a whole tree into a self-referencing table, parents after children;
- delete a whole subtree.

Checks are not deferred to commit. Tables that refer to each other in a cycle need a nullable column and two statements. Deferred constraints are future work.

**Violations fail the statement** with the new SQLSTATE `23503`, `foreign_key_violation`. Statement atomicity (ADR-0026) rolls the statement back, and the transaction stays usable.

### How the DML operator enforces them

**Plan time.** The physical planner attaches a `ReferencePlan` to the DML operator, as it attaches a `ClosurePlan` (ADR-0032). The plan holds:
- the target's *outgoing* foreign keys, for inserts and updates;
- its *incoming* foreign keys, for deletes and updates of referenced columns;
- for each `Cascade` or `SetNull` foreign key that the statement can reach, a prepared cascade write (described below).

`EXPLAIN` shows `checking foreign keys: ...` and `cascading to: ...`.

**Per batch.** The checks run as each batch is written, using the same batched reads as the unique checks:

- **Outgoing.** The operator collects the distinct referencing keys of the batch's new rows and probes the referenced table for all of them in one storage call. The probe reads the latest state, including the statement's own writes. A key that's found is done. A key that's missing goes into a *pending* set, charged to the query's memory budget, because a later batch of the same statement may insert it.
- **Incoming.** For the batch's deleted or re-keyed referenced rows, the operator probes the referencing table's index by key prefix. A key with no referencing rows is done. A referencing row that's found is handled by the foreign key's action:
  - `NoAction`: it goes into the pending set, because a later batch may delete it;
  - `Cascade` or `SetNull`: it is queued for the cascade.

In the usual case every key is found, or has no referencing rows, at once, so the pending sets stay empty. A bulk insert of children whose parents already exist holds no extra memory.

**Statement end.**
1. Run the cascade queue until it is empty. Cascaded writes can queue more cascades and add pending checks.
2. Recheck every pending key. Any still unsatisfied fails the statement with `23503`.

**Storage.** The row layer gains two functions. Each reads the latest state and registers what it read as a dependency, so a check and its dependency can't diverge:
- `rows::probe_keys(txn, table, index, keys) -> Vec<bool>` probes primary keys or unique index entries;
- `rows::probe_prefixes(txn, table, index, prefixes) -> Vec<Vec<Row>>` scans index ranges, with a limit.

### Concurrency

The checks register their reads as dependencies (`register_row_dependency`, `register_scan_dependency`). As with closure maintenance, a conflicting concurrent change then fails the later committer with `40001` at every isolation level:

| Race | The earlier committer | Why the later one fails |
| --- | --- | --- |
| T1 inserts a child of P; T2 deletes P | T2 | T1 depends on P's row key, which T2 wrote |
| | T1 | T2 depends on the index range of P's children, where T1 wrote an entry |
| T1 changes a child to refer to P; T2 re-keys P | either | the same two dependencies |

A retry sees the other transaction's write and either succeeds or fails with `23503`.

**Accepted cost.** A child insert depends on its parent's row key, so it also conflicts with a concurrent update of any column of that parent, not just its key. Postgres avoids this with `FOR KEY SHARE` row locks. Here it costs a retry. Narrower dependencies are future work.

### Cascades

- **`Cascade`:** deleting a referenced row deletes the rows that refer to it.
- **`SetNull`:** deleting a referenced row sets the referencing columns to NULL. Under ADR-0034 this becomes `SetNone`, on option columns.
- **Updates of referenced columns** are `NoAction` only. `ON UPDATE CASCADE` is future work.

**Cascaded writes are ordinary DML.** The planner prepares one secured write plan per cascading foreign key that the statement can reach. Each plan is a delete, or an update for `SetNull`, of the referencing table, keyed by an index lookup on the referencing columns. The graph of foreign keys can contain cycles, such as a self-referencing folder tree. So the plans are prepared once each and reused, and the executor runs a work list. Each row is changed at most once per statement, as today (`DmlState::changed`), so the work list ends. Each cascaded write:
- passes the principal's `WriteCheck` for the referencing table (ADR-0018, ADR-0025);
- maintains closures if the referencing table is a membership edges table. Deleting an entity then cascades to its membership edges and corrects the closure;
- checks its own foreign keys and queues its own cascades;
- is charged to the same memory budget and rolled back with the statement.

**A cascade removes every referencing row or fails.** Referencing rows are found without a `PolicyFilter`: what the constraint requires can't depend on who is asking. Each row found must pass the principal's delete (or update) check. If one doesn't, the statement fails with `42501` naming the referencing table, without row values. Postgres runs cascades with row security bypassed. Interlaken doesn't, because then deleting one row it may delete would let a principal delete rows its policies forbid.

### Security

- **Checks don't apply read policies.** A foreign key check reads the referenced or referencing row whether or not the writer may read it. Otherwise one constraint would mean different things to different principals.
- **This is an existence channel**, the same one unique indexes already open, and Postgres has too:
  - inserting a row that refers to key k succeeds only if a row with key k exists;
  - deleting a row fails while invisible rows still refer to it.

  The book's security chapters will say so, with the same advice as for unique indexes: don't put a foreign key across a boundary that must hide existence.
- **Error messages carry no values of rows the writer can't read.**
  - An outgoing violation names the constraint and the writer's own referencing values.
  - An incoming violation names the constraint and the referencing table only.
- **Cascades don't escalate privileges:** see above.

### The optimizer

Foreign keys give the optimizer two facts:
- a child joins to at most one parent;
- a child whose referencing columns are NOT NULL joins to exactly one.

These facts allow:
- **Join elimination.** Remove an inner join from child to parent on the foreign key when no parent column is used and the referencing columns are NOT NULL. This is valid only when the parent table has no `PolicyFilter`. With one, the join also removes children whose parent the principal can't see, and eliminating it would expose them. Elimination runs after security lowering, which inserts the filters (ADR-0018).
- **Cardinality estimates** for such joins (ADR-0014, ADR-0028).

## Implementation Phases

- [ ] **Phase 1: Declarations.** `ForeignKeySpec`; `create_foreign_key` and `drop_foreign_key`, including the existing-row check; the two engine catalog tables; `drop_table` and `drop_index` guards; `ForeignKeyDef` in the IR catalog and catalog text; agreement with `references`; SQLSTATE `23503`; the book's DDL chapter and catalog table reference.
- [ ] **Phase 2: Checks.** `rows::probe_keys` and `probe_prefixes` with dependency registration; `ReferencePlan`; per-batch checks and the statement-end recheck of pending keys; `EXPLAIN`.
    - **Oracle test:** random DML statements on a parent, a child and a self-referencing table, through `Session`. After every statement, the outcome must match a model that applies the statement and then checks integrity from scratch: rejected if and only if the model finds a violation, and with no dangling references in any case.
    - **Concurrency test:** sessions race to insert children and delete parents at each isolation level. After all commits, no reference dangles, and every failure is `40001` or `23503`.
- [ ] **Phase 3: Cascades.** `Cascade` and `SetNull`: prepared cascade plans, the work list, write checks, closure maintenance through cascades, `42501` for denied cascaded rows. The oracle gains cascading foreign keys and an entity whose deletion cascades to its membership edges.
- [ ] **Phase 4: Optimizer.** Join elimination for ungoverned parents, and foreign-key cardinality estimates, with property tests that eliminated and original plans agree.

## Consequences

### Positive

- Referential integrity holds at every isolation level, which applications can't achieve themselves under snapshot isolation.
- Deleting an entity can remove its membership edges, keeping closures free of entities that no longer exist.
- Cascades respect write policies, so a foreign key never lets a principal change rows they couldn't change directly.
- GraphQL relationships and decomposed schemas get a declared, checked basis.
- The optimizer gains join elimination and better join estimates.

### Negative

- Writes to tables with foreign keys do extra reads: one batched probe per batch for each foreign key that applies.
- Spurious `40001` conflicts between child inserts and concurrent non-key updates of their parent.
- An existence channel across foreign keys, as with unique indexes.
- The required referencing index costs storage and write time even where deletes of the parent are rare.
- Cascades can write many rows in a statement the client thought touched one, all within its memory budget.

## Open Questions

- Should `RESTRICT` (checked per row, immediately) be distinct from `NO ACTION`, or should the SQL front end lower `RESTRICT` to `NO ACTION`? They differ only within a statement.
- Should DDL create the referencing index automatically instead of requiring it?
- Should declaring a `references` attribute offer to create the matching foreign key?
- Is `MATCH SIMPLE` the right default once ADR-0034's option types land, or should partly absent composite keys always be violations?

## Future Work

- Deferred constraints, checked at commit, for tables that refer to each other in a cycle.
- `ON UPDATE CASCADE` and `ON UPDATE SET NULL`.
- Narrower dependencies, so a child insert doesn't conflict with a concurrent update of its parent's non-key columns.
- `ON DELETE SET DEFAULT`, if columns gain defaults.

## References

- [ADR-0007](0007-minimal-catalog-and-row-layer.md), [ADR-0012](0012-nesting-and-decorrelation-for-graphql.md), [ADR-0014](0014-rule-based-optimizer.md), [ADR-0017](0017-entities-and-hierarchy-closure-table.md), [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md), [ADR-0026](0026-statement-atomicity-and-halloween-protection.md), [ADR-0028](0028-physical-operators-planning-and-explain.md), [ADR-0031](0031-engine-sessions-and-storage-backed-catalog.md), [ADR-0032](0032-authorization-completeness.md), [ADR-0034](0034-two-valued-logic-with-an-option-type.md)
- Postgres documentation: foreign keys, `MATCH SIMPLE` and `MATCH FULL`, and referential actions.
