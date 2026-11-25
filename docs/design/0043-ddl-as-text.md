---
id: 0043
title: DDL as Text
status: implemented  # proposal | accepted | implemented | superseded
tags: [ir, engine, catalog]
feature-flag:
created: 2026-11-25
accepted: 2026-11-25
implemented: 2026-11-25
spec-sections: []
superseded-by:
---

# ADR-0043: DDL as Text

## Status

Implemented, in both phases. The *As implemented* notes after the phases record where the result differs from this design, chiefly: the DDL statement types and specs live in the IR crate; namespace and principal-type operations have forms too; `query_stream` refuses DDL; and redeclaring a principal type changes nothing.

## Summary

Queries and DML are IR text (`(plan ...)`), but every schema change is a Rust call with a spec struct: `create_table(&TableSpec)`, `declare_entity(&EntitySpec)` and so on. That works for programs that build schemas in code. It doesn't work for schemas kept as files, such as migrations shipped with an application, applied by a tool, reviewed in a diff, or written by someone who doesn't write Rust. Every such program ends up writing its own text format over the Rust API.

The IR already has a text form for catalogs, `parse_catalog`, but only for in-memory test fixtures: it carries internal IDs and builds an `InMemoryCatalog` without touching the engine.

This ADR decides:

1. **DDL statements in IR text,** one form per Rust DDL operation, using `parse_catalog`'s vocabulary without IDs.
2. **They run like any statement** through `Session::query`, inside the current transaction, with the same privileges and errors as the Rust calls.
3. **Scripts:** `Session::execute_script(text)` runs a sequence of statements of any kind in order, in the current transaction or in one implicit transaction.
4. **Printing the schema** as DDL text, `Session::schema_text()`, which round-trips through scripts.

SQL DDL stays with the SQL front end, which can lower into the same operations.

## Context

### The Rust API

| Operation | Spec |
| --- | --- |
| `create_table`, `drop_table` | `TableSpec { name, columns, primary_key }` |
| `add_column` | `ColumnSpec { name, ty }`; the type must be optional |
| `create_index`, `drop_index` | `IndexSpec { name, table, columns: Vec<IndexKey>, unique, nones_distinct }` |
| `declare_entity`, `drop_entity` | `EntitySpec { name, table, key, attributes }` |
| `declare_membership`, `drop_membership` | `MembershipSpec { member, group, edges, edge_columns, closure, closure_columns }` |
| `create_foreign_key`, `drop_foreign_key` | `ForeignKeySpec { name, table, columns, references, referenced_columns, on_delete, match_full }` |
| `set_function_leakproof` | a function name and argument types |

Each runs as one statement through `Session::run`: in the open transaction if there is one, otherwise in an implicit one. Each checks the `admin` privilege, reloads the catalog to validate the result, and bumps the schema version. DDL and queries can share a transaction (ADR-0031).

Policy changes (`submit_policies`, `verify_policies`, `activate_policies`, `set_policy_settings`), privileges (`grant`, `revoke`) and cluster membership are Rust calls too.

### The IR text

- `Session::query`, `prepare` and `explain` parse with `parse_plan`, which accepts exactly `(plan [:schema-version N] REL)`. DML is relational operators inside `plan`. There is no other statement form.
- `parse_catalog` reads `(catalog (table NAME :id N (column ...) (primary-key ...) (index ...)) (entity ...) (membership ...) (foreign-key ...) (function ...))`. Tables need `:id`, and indexes are nested in tables.
- Nothing prints a catalog back to text.
- The book's DDL chapters use Rust examples only. `ir` blocks are checked by a test that prepares each form against the example database; the same test could check DDL.

## Decision

### 1. Statement forms

Each form is a top-level statement, like `(plan ...)`. Item syntax follows `parse_catalog`, minus IDs; indexes are their own statements.

```lisp
(create-table todos
  (column id text) (column owner text) (column title text) (column done bool)
  (primary-key id))
(add-column todos note (option text))
(drop-table todos)

(create-index todos_by_owner :table todos owner (desc done :nulls last) :unique false)
(drop-index todos_by_owner)

(declare-entity Todo :table todos :key id (attr owner :references User) (attr done))
(drop-entity Todo)

(declare-membership User Group :edges user_groups (member grp)
                               :closure user_group_closure (descendant ancestor paths))
(drop-membership User Group)

(create-foreign-key todos_owner :table todos (columns owner) (references users id)
                    :on-delete cascade :match simple)
(drop-foreign-key todos_owner)

(set-leakproof lower (text) true)
```

- **Types** are IR types, parsed by `parse_type`.
- **Keywords** default as in the Rust API. Unknown keywords and missing required parts are syntax errors (`42601`) with the position, as for plans.
- **Verbs** are explicit (`create-`, `drop-`, `declare-`), because `(table ...)` in a statement reads as a declaration of the final schema, which these are not (see Future Work).
- **Names** are identifiers, and become `ns.name` when namespaces exist (ADR-0042).

Each form parses into the existing spec struct and calls the existing operation, so text and Rust can't drift: one implementation, two front ends.

### 2. Running statements

- `parse_plan` becomes `parse_statement`, returning `Statement::Plan(Plan)` or `Statement::Ddl(DdlStatement)`.
- `Session::query` runs either. A DDL statement returns an empty result with no columns.
- `prepare` and `explain` refuse DDL with `0A000`: DDL has no plan to keep or show.
- Privileges, catalog validation, schema version bumps and errors are those of the Rust call.

### 3. Scripts

```rust
let report = session.execute_script(include_str!("migrations/0003_todos.ir")).await?;
```

- **A script is a sequence of statements**, DDL and `(plan ...)` mixed, with `;` comments to the end of the line.
- **Transactions:** in an open transaction, the script runs in it. Otherwise it runs in one implicit transaction, so it applies entirely or not at all.
- **Errors** name the failing statement's position and form (`(create-index todos_by_owner)` at line 12), and roll back an implicit transaction. In an open transaction, the failing statement is undone (ADR-0026) and the error is returned; the caller decides whether to roll back.
- **The report** lists each statement's kind and row count.
- **No migration bookkeeping** is built in. Which scripts have run is the application's business, recorded in its own table in the same transaction.

### 4. Printing the schema

`Session::schema_text()` prints the user schema as a script of `create-` and `declare-` statements, in dependency order: tables, indexes, entities, memberships, then foreign keys. Running it on an empty database recreates the schema. A test round-trips it.

### 5. In the book

The DDL chapters gain `ir` examples next to the Rust ones. The book test runs DDL blocks in a transaction it rolls back, so they're checked against the example database without changing it.

## Alternatives considered

- **Use `parse_catalog` forms as statements** (`(table ...)`). It's closer to the fixtures, but a declaration reads as "the schema is this", which a later `(table ...)` for an existing table would contradict.
- **SQL DDL first.** SQL is the familiar choice, but the SQL front end doesn't exist. IR text DDL is a small addition now, and the SQL front end can lower `CREATE TABLE` into the same specs later.
- **A separate migration format** outside the statement language. It would need its own parser and runner, and couldn't mix DDL with the DML that migrations often need (backfilling a column).
- **Declarative schemas** (apply a target schema; the engine works out the changes). Convenient, but it needs diffing, rename detection and a policy for destructive changes. It can be built on these statements later.

## Testing

- **Round trip:** every form parses, prints and reparses identically; random catalogs printed with `schema_text` and replayed on an empty database produce equal catalogs.
- **Parity:** each form gives the same catalog and the same errors as its Rust call, including invalid definitions (`42P16`) and missing privileges (`42501`).
- **Scripts:** mixed DDL and DML; a failure halfway rolls back an implicit transaction completely; in an open transaction, the failing statement is undone and earlier ones stay.
- **The book's DDL examples** are checked as `ir` blocks.

## Implementation Phases

### Phase 1: statements

- `parse_statement` and every DDL form; `Session::query` running them; `prepare` and `explain` refusing them.

### Phase 2: scripts and printing

- `execute_script` and its report; `schema_text`.
- The book: DDL examples in IR text, and a section on migration scripts.

### As implemented

- **Where the types live.** The spec structs (`TableSpec`, `IndexSpec`, ...) moved from the engine to `interlaken_ir::ddl`, beside `DdlStatement` and `Statement`, so the IR text reader can produce them. `interlaken_engine::catalog` re-exports them under their old paths.
- **`parse_plan` stays.** It still accepts only plans. `parse_statement` is added beside it, with `parse_script`, `print_statement` and `print_script`. The engine's own parsing goes through `parse_statement`.
- **More forms.** ADR-0042's operations got forms too: `create-namespace`, `set-namespace-owner`, `drop-namespace`, `declare-principal-type` and `drop-principal-type`. An owner is written `(entity TYPE KEY)`.
- **`set-leakproof`** takes its argument types as a plain list, `(set-leakproof lower (text) true)`.
- **Canonical layout.** The printer puts positional parts before keyword arguments when a statement fits on one line, so `(create-index i :table t a)` prints as `(create-index i a :table t)`. Both forms parse.
- **Streaming.** `query` runs DDL. `query_stream` refuses it with `0A000`, as `prepare` and `explain` do, since there are no rows to stream. A DDL statement given parameters fails with `08P01`.
- **Script errors** are `EngineError::Script { index, form, line, column, source }`, with the SQLSTATE of `source`. In an open transaction, a failing statement aborts the transaction as any statement does (ADR-0026); the caller rolls back, or rolls back to a savepoint taken before the script.
- **Principal types are idempotent.** `schema_text` prints every principal type, including the first administrator's, which every new database already has. So that its output replays, declaring a principal type that is already declared now succeeds and changes nothing, from the Rust API too. Declaring one with an entity type's name still fails with `42P16`.
- **Child names.** An index, entity type or foreign key named with a namespace (`app.by_owner`) must be in its table's namespace, which `schema_text` relies on; before this, the namespace was added a second time.
- **What `schema_text` leaves out:** rows, policies, privileges and the owner of `public`. Names in `public` are unqualified, so the script is meant for the default search path.
- **Testing.** Property tests round-trip every form and scripts of them through print and parse. The engine tests check that text and Rust give the same catalog, that the schema round-trips through a new database, scripts' atomicity and reports, and the errors. The book's `ir` blocks holding DDL run as scripts in a rolled-back transaction.

## Consequences

### Positive

- Schemas and migrations can live in files and be applied without Rust code for each change.
- One implementation behind both front ends.
- `schema_text` gives a reviewable dump of a database's schema.

### Negative

- The statement language grows beyond plans, and `parse_plan` callers move to `parse_statement`.
- Two spellings of the same items (`parse_catalog` fixtures and DDL statements) must be kept consistent.

## Open Questions

- Should policy, privilege and settings changes get text forms too (`(submit-policies :language cedar "...")`), or stay Rust calls?
- Should `execute_script` accept parameters, for scripts that insert configured values?

## Future Work

- Declarative schemas built on `schema_text` and the statement forms.
- Dropping, renaming and retyping columns, when the catalog supports them (both front ends get them together).
- SQL DDL in the SQL front end.

## References

- [ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md): the IR text forms and `parse_catalog`.
- [ADR-0026](0026-statement-atomicity-and-halloween-protection.md): statement atomicity.
- [ADR-0031](0031-engine-sessions-and-storage-backed-catalog.md): DDL in sessions.
- [ADR-0042](0042-namespaces-and-scoped-policies.md): qualified names.
