---
id: 0031
title: Engine Sessions and the Storage-Backed Catalog
status: implemented  # proposal | accepted | implemented | superseded
tags: [engine, catalog, security, api]
feature-flag:
created: 2026-11-20
accepted: 2026-11-20
implemented: 2026-11-20
spec-sections: ["mvp-completion.md#f1-engine-integration", "ir-design.md#names-resolution-and-formats"]
superseded-by:
---

# ADR-0031: Engine Sessions and the Storage-Backed Catalog

## Status

Implemented

## Summary

The `interlaken-engine` crate is the database's public API:
- **`Engine`:** opens one database.
- **`Session`:** runs one principal's statements in transactions. Queries are IR text or built plans, run through resolve, verify, secure, optimize, plan and execute in one transaction.
- **DDL:** typed session methods that only administrators may call.

The IR catalog is read from storage's `sys_*` tables plus engine-owned `engine_*` tables, all in the statement's transaction. Those tables hold IR column types, entity and membership declarations, function property overrides, privileges and a catalog-wide schema version. Loaded catalogs are cached by schema version. Every DDL statement bumps the version, which pins resolved plans and makes concurrent DDL and writes conflict.

## Context

After milestones M1–M6, I1–I5 and E1–E5, each layer worked and was tested on its own, but nothing joined them:
- The IR catalog lived only in memory, built by tests.
- No API took a statement and a principal.
- Policies, privileges and the residual cache had no owner that applied them to every statement.

ADR-0013 deferred its storage-backed catalog (phase 3) to this point. Two backlog items needed roles before they could close:
- `leakproof-admin-only` (ADR-0011): only administrators may mark functions leakproof.
- `explain-policy-markers` (ADR-0025): exact redaction in `EXPLAIN`.

`ir-design.md` says DDL isn't part of the IR: "catalog changes go through the catalog API, inside a transaction".

## Decision

### Catalog storage

Storage's `sys_*` tables keep table, column and index definitions (ADR-0007). The engine adds its own tables, created by `Engine::create` and changed in the same transactions:

| Table | Primary key | Other columns |
| --- | --- | --- |
| `engine_catalog` | id (always 1) | schema_version |
| `engine_columns` | table_id, column_id | type (IR type text) |
| `engine_entities` | name | table_id, key_column |
| `engine_attributes` | entity, position | name, column_id, refs |
| `engine_memberships` | member, grp | edges, edge_member, edge_group, closure, closure_descendant, closure_ancestor, closure_paths |
| `engine_functions` | function_id | leakproof |
| `engine_privileges` | principal_type, principal_key, privilege | |

The policy tables (ADR-0020, ADR-0030) are renamed into the same namespace: `engine_policy_set`, `engine_policy_settings`, `engine_policy_proposals` and `engine_policy_verifications`.

- **Reserved namespace.** Names starting with `engine_` are reserved. Engine DDL rejects them, and the IR catalog leaves engine tables out. Clients can neither read nor write them, policy tables included, except through the engine's API. Storage's `sys_*` tables were never in the IR catalog.
- **Identifiers.**
  - IR table IDs are storage table IDs.
  - IR column IDs are column positions.
  - Built-in functions stay in code. Their properties can be overridden per function ID.
- **IR column types.** Each column's IR type is recorded as text, because storage types lose information: an IR array's element nullability, for example. A column without a recorded type (one created by storage DDL directly) gets the IR namesake of its storage type. Nullability always comes from storage.
- **Schema version.** One catalog-wide version, starting at 1, incremented by every engine DDL statement.

### Loading the catalog

`catalog::load_catalog(txn)` builds an `InMemoryCatalog` from these tables:
- the built-in functions, with any property overrides;
- the user tables;
- the entity types and memberships.

Its checks also validate the declarations, so the in-memory and storage-backed catalogs behave identically by construction.

Every read goes through the transaction. The schema version row is also registered as a dependency (as `load_policies` does for the policy row, ADR-0019), so a writing transaction conflicts with concurrent DDL at every isolation level. A plan therefore never commits writes against a catalog that changed under it.

**The cache.** The engine keeps the catalogs of the last four committed schema versions. A statement:
1. reads the schema version through its transaction;
2. takes the cached catalog at that version, or loads and caches it.

Concurrent DDL conflicts on the version row, so each committed version names exactly one catalog. A transaction that ran DDL sees an uncommitted version, which another transaction may also reach with different DDL. It neither reads nor fills the cache, and loads the catalog for every statement.

### DDL

DDL is a set of typed functions in `catalog`, wrapped by `Session` methods:
- `create_table`, `drop_table`, `add_column`;
- `create_index`, `drop_index`;
- `declare_entity`, `drop_entity`;
- `declare_membership`, `drop_membership`;
- `set_function_leakproof`.

Each function:
- changes storage's catalog through storage DDL and the engine's tables through the row layer;
- checks names and references (for example, a table backing an entity type can't be dropped);
- bumps the schema version.

The session then reloads the catalog within the statement, which checks that the declarations fit together. A failure undoes the whole statement.

### Privileges

- **`admin`:** DDL (including leakproofness, which decides what may move below a security barrier), policy proposals and activation, settings, and grants.
- **`read-policies`:** see policy logic in `EXPLAIN`. `admin` implies it.

The principal passed to `Engine::create` is the first administrator. Privileges are read through each statement's transaction.

Privileges don't exempt anyone from policies. Administrators' queries are governed like everyone's.

### Sessions and the statement pipeline

`Engine::session(context)` takes a `SecurityContext` whose principal the caller has authenticated.

**Transactions.**
- Without an open transaction, each statement runs in its own (autocommit).
- With one open, statements run in it with ADR-0026's semantics. Each statement is atomic, and a failure aborts the transaction until a rollback to a savepoint or of the whole transaction.
- Every session statement goes through the executor's `Transaction::statement`, so DDL and policy changes get the same semantics as queries.

**The pipeline.** For a query, in one transaction:
1. Parse the IR text (or take a built plan).
2. Resolve it against the transaction's catalog, unless it is already resolved: a plan pinned to a schema version, as a prepared statement keeps it.
3. Verify it as untrusted client IR. This rejects plans pinned to an older schema version (SQLSTATE `0A000`), privileged operators and the reserved policy marker.
4. Load the principal's entities (ADR-0017) and the active policies through the transaction.
5. Secure the plan with the policies' provider. The provider for the newest committed catalog and policies is reused, and residuals are cached (ADR-0019). Neither cache is used after DDL or a policy change in the same transaction.
6. Optimize it.
7. Plan it physically and execute it with the transaction's `now()`, the session's memory budget and the node pool (ADR-0027).

Results carry column names, IR types and Arrow batches. `Session::prepare` stops after step 3 and returns the pinned plan.

**`EXPLAIN [ANALYZE]`.** Runs the same pipeline. Policy logic is shown only with `read-policies`.

**`statement_timeout`.** Cancels a statement from outside in two ways:
- The whole statement runs under a Tokio timeout, for statements waiting on storage.
- The executor context carries a deadline that every operator's stream checks at each batch, so CPU-bound statements stop on time even though operators don't yet yield cooperatively.

A cancelled statement fails with SQLSTATE `57014`, its writes are undone, and an explicit transaction is aborted.

### Exact `EXPLAIN` redaction

Security lowering marks everything it adds:
- every table it reads in a policy subquery is aliased with the reserved prefix `sec$` and a number;
- every literal relation it builds has its columns named the same way.

The verifier rejects the prefix in untrusted plans, so the marker can't be forged. The physical planner records the marker on scans and `Values`.

`EXPLAIN` collapses any subplan whose leaves all carry the marker, wherever the optimizer put it beneath a policy filter or write check. It hides the condition of any join with such a side. Beneath a policy filter, every expression is hidden.

This replaces the shape-based rule of E5 (hide the inner side of joins on the governed path), which an optimizer change could have defeated. Under a write check, the shape-based rule had shown the policy's subplans.

### Column types

Storage gains `date` (as `int4`), `timestamptz` (as `int8`), `uuid` (16 bytes) and `jsonb` (text) in the tuple encoding, the row format and the catalog codes, with arrays of each.
- `jsonb` isn't keyable, matching the IR's lack of `jsonb` comparison.
- Arrow carries `jsonb` as text, so DML writes text bound for a `jsonb` column as `jsonb`, and `QueryResult::rows` types values by the result's IR types.
- After this, `stored_type` covers every IR scalar type and arrays of them.

## Implementation Phases

- [x] **Phase 1: Storage temporal types, the storage-backed catalog and DDL, privileges, sessions, timeouts and exact `EXPLAIN` redaction** - F1
- [x] **Phase 2: Streaming results** - backlog `engine-streaming-results`
  - *As implemented:* `Session::query_stream(text, params)` (and `query_plan_stream`) plans the statement and returns a `QueryStream<'_>`, which borrows the session mutably, so the borrow checker stops the session running anything else while a stream is live. It reports `columns()` and `types()` before running, and `next().await` pulls one Arrow batch from the executor's lazy stream at a time. `finish()` runs the rest discarding rows, and `collect()` gathers a `QueryResult`; `Session::query` and `query_plan` are `query_stream(...).collect()`. The statement keeps ADR-0026's semantics:
    - **Statements.** `exec::Transaction` gains a lower-level statement API: `begin_statement()` (the aborted check, `next_statement()` and the implicit savepoint) returns a `Statement` token, ended by `release_statement` (keep the writes), `rollback_statement` (undo them and abort the transaction) or `cancel_statement` (undo them only). `Transaction::statement` is built on it. A stream begins its statement before planning and holds the token until it ends.
    - **The end of the rows** releases the statement, and commits the implicit transaction in autocommit; a commit failure (`40001`) is returned by the last `next()`.
    - **An error** while planning or streaming, the timeout included, rolls the statement back and aborts an explicit transaction, or rolls back the implicit one. The error is returned once and the stream ends.
    - **Dropping the stream early** depends on whether the statement writes (`Physical::writes`):
      - *A query* stops: the executor's stream is dropped first, which stops execution, and the statement is as if it had never run. In autocommit the implicit transaction is rolled back. An explicit transaction is not aborted, since the caller chose to stop reading, which isn't a failure.
      - *A DML statement* runs to its end anyway, discarding its remaining rows, as Postgres runs a statement's writes to completion whether or not the client reads its `RETURNING` rows. A client that reads a few returned rows and moves on can't lose its update. `Drop` can't wait, so the stream moves the statement's remaining batches, with its deadline, and the open transaction into a Tokio task. The task ends the statement as `next` would: it keeps the writes and commits an implicit transaction, or undoes them and aborts the transaction (rolling back an implicit one) if the statement fails or times out. The session records the task, and every call that touches the transaction (`begin`, `commit`, statements, DDL, savepoints, `is_aborted`) first waits for it and takes back the explicit transaction, so the next statement sees the writes. A failure is reported to no one: it aborts an explicit transaction, so the commit then fails with `25P02`, and an implicit transaction's writes are simply not committed. `rollback` stops the task, since its writes would be discarded anyway, and an autocommit statement commits even if the session is dropped before its next call. Without a Tokio runtime on the dropping thread, the statement is undone as a query is.
      - *Decided (2026-11-20):* the first implementation undid a dropped DML statement as for a query, so an `UPDATE ... RETURNING` read part way kept none of its writes. Aborting the explicit transaction on drop was the other alternative; it can't lose a write silently, but it turns stopping reading into a failure. Finishing DML matches what clients expect from Postgres.
      - `savepoint`, `rollback_to`, `release` and `is_aborted` became `async` so they can wait for a finishing statement.
    - **`statement_timeout`** runs from the `query_stream` call, wall-clock, including time the caller spends between batches. Planning runs under a Tokio timeout, each `next()` under `tokio::time::timeout_at` the deadline, and the executor context carries the same deadline, checked at every batch.
  - *Testing:* `session_streaming.rs` covers a stream read to the end (several batches, nothing visible to other sessions until it ends, then committed), dropped early (a query: autocommit rolled back, an explicit transaction still usable; DML: run to its end in the background whether part read or never read, committed in autocommit even if the session is dropped, kept in an explicit transaction until it commits, a failure while finishing aborting it, the next call waiting for it (`begin` refused as a transaction is in progress, DDL running in that transaction), and `rollback` stopping it), `finish`, an error in a later batch (autocommit rolled back; explicit transaction aborted, `25P02` after, commit refused), planning errors, and the timeout both while computing and when read after the deadline (writes undone, explicit transaction aborted). A sort's later batches, computed before the deadline and read after it, must fail too, and so must a stream whose storage scans return no rows. A `compile_fail` doc test shows the session can't run a statement while a stream borrows it. Planted bugs are each caught: a dropped DML statement undone like a query, `run`, `begin` or `is_aborted` not waiting for a finishing statement, `rollback` not stopping it, a failure while finishing keeping its writes, an implicit transaction not committed after finishing, an error that cancels instead of aborting, an error that leaves the implicit transaction open, no deadline in the executor context, and no commit at the end. Removing only the Tokio timeout around pulls is not caught: the executor's per-batch deadline stops every stream the tests can build, scans that return no rows included, so the two layers overlap; the Tokio layer remains for a single long wait on storage.
  - *Memory:* `streaming_memory.rs` installs the counting allocator. Streaming 150,000 of 300,000 rows (4.2 MB of batches) through a session peaks at about 0.35 MB of heap growth, while collecting them holds 4.3 MB; an aggregation into 1,000 groups peaks at 0.7 MB. The on-demand variant (`--ignored`, `STREAMING_ROWS`, default 3 million) streams 1.5 million rows (42 MB of batches) with 0.6 MB of peak growth and the aggregation with 1.3 MB; at 10 million rows the aggregation peaked at 4 MB. This is the session-level counterpart of `crates/exec/tests/ten_million.rs`, which stays at the executor level for its resident-set and budget checks.

## Consequences

### Positive
- One public API runs every statement through the whole pipeline, so no path skips verification or security lowering.
- Catalog, data and policies change in transactions together and survive restarts.
- Plans pinned to old schema versions fail instead of running against a changed catalog.
- Clients can't reach the engine's own tables, policy tables included.
- `EXPLAIN` redaction no longer depends on plan shape.

### Negative
- Any DDL conflicts with every concurrent writing transaction, not just those touching the changed tables. DDL is rare in the target workloads, and Postgres takes similar locks for most DDL.
- A cache miss loads the whole catalog, and a transaction that ran DDL loads it for every later statement.
- Databases created before F1 aren't migrated (the risk accepted in `mvp-completion.md`).
- Residual cache keys include the policy version but not the schema version. Residuals depend on policies and principal data, and lowering against a changed catalog fails closed if a residual refers to something no longer declared.

## Open Questions

- Should DDL conflict only with transactions that read the changed tables? That needs per-table versions in plans and catalog reads registered per table.

## Future Work

- A wire protocol and the SQL front end on top of `Session`.
- User-defined functions in the catalog (ADR-0029 phase 2), with leakproofness still admin-only.
- Finer-grained privileges (per-table DDL, delegated policy authorship).

## References

- [ADR-0007](0007-minimal-catalog-and-row-layer.md), [ADR-0011](0011-function-property-catalog.md), [ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md), [ADR-0017](0017-entities-and-hierarchy-closure-table.md), [ADR-0019](0019-transactional-authorization-and-residual-cache.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md), [ADR-0026](0026-statement-atomicity-and-halloween-protection.md), [ADR-0027](0027-query-memory-management.md), [ADR-0028](0028-physical-operators-planning-and-explain.md), [ADR-0030](0030-policy-proposals-and-distributed-verification.md)
- `mvp-completion.md` — F1
