# Schema scripts

Every DDL method of `Session` has a statement form in IR text, such as `(create-table ...)` for `create_table`. A schema can therefore live in files, such as migrations shipped with an application, reviewed in a diff and applied without Rust code for each change. The statements and the methods share one implementation, so they take the same privileges, check the same things and fail with the same SQLSTATEs.

## DDL statements

| Statement | Method |
| --- | --- |
| `(create-table NAME (column NAME TYPE)... (primary-key NAME...))` | `create_table` |
| `(add-column TABLE NAME TYPE)` | `add_column` |
| `(drop-table NAME)` | `drop_table` |
| `(create-index NAME :table TABLE KEY... [:unique B] [:nones-distinct B])` | `create_index` |
| `(drop-index NAME)` | `drop_index` |
| `(declare-entity TYPE :table TABLE :key COLUMN (attr NAME [COLUMN] [:references TYPE])...)` | `declare_entity` |
| `(drop-entity TYPE)` | `drop_entity` |
| `(declare-principal-type TYPE)`, `(drop-principal-type TYPE)` | `declare_principal_type`, `drop_principal_type` |
| `(declare-membership MEMBER GROUP :edges TABLE (M G) :closure TABLE (D A P))` | `declare_membership` |
| `(drop-membership MEMBER GROUP)` | `drop_membership` |
| `(create-foreign-key NAME :table TABLE (columns C...) (references TABLE C...) [:on-delete A] [:match M])` | `create_foreign_key` |
| `(drop-foreign-key NAME)` | `drop_foreign_key` |
| `(set-leakproof FUNCTION (TYPE...) B)` | `set_function_leakproof` |
| `(create-namespace NAME :owner (entity TYPE KEY))` | `create_namespace` |
| `(set-namespace-owner NAME (entity TYPE KEY))` | `set_namespace_owner` |
| `(drop-namespace NAME [:cascade B])` | `drop_namespace` |

The chapters on [tables](tables.md), [indexes](indexes.md), [entities](entities.md), [foreign keys](foreign-keys.md) and [namespaces](namespaces.md) show each form next to its method. Options left out take the methods' defaults. Unknown keywords and missing parts fail with `42601`, giving the line and column.

`Session::query` runs a DDL statement like any other statement, in the open transaction or in its own. It takes no parameters and returns no columns:

```rust,ignore
session.query("(add-column docs summary (option text))", Vec::new()).await?;
```

`prepare`, `explain` and `query_stream` refuse DDL with `0A000`, since DDL has no plan.

## Running a script

`Session::execute_script` runs a sequence of statements, DDL and `(plan ...)` mixed, in order. `;` starts a comment that runs to the end of the line:

```rust
# use interlaken_engine::{Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));
let migration = r#"
; 0001: notes, with a title index.
(create-table notes (column id int8) (column title text) (primary-key id))
(create-index notes_by_title :table notes title)
; A row the application expects.
(plan (insert notes (values (columns a b) (row 1 "Welcome")) (columns id title)))
"#;
let report = session.execute_script(migration).await?;
assert_eq!(report.statements.len(), 3);
assert_eq!(report.statements[2].form, "plan");
# Ok(())
# }
```

- **Atomicity.** With no transaction open, the script runs in one transaction of its own, so it applies entirely or not at all. In an open transaction, its statements run in that transaction. A failing statement is undone and aborts the transaction, as any failing statement does (see [Statements and errors](../transactions/statements.md)), and the caller rolls back.
- **Errors** are `EngineError::Script`, naming the failing statement's position in the script, its form and its line, and carrying the failure's SQLSTATE. A script that doesn't parse fails with `42601` before anything runs.
- **The report** lists each statement's form, position and the number of rows it returned. It also gives the commit timestamp when the script ran in its own transaction.
- **Which scripts have run** isn't recorded. A migration tool keeps that in a table of its own, written by the same script so it commits with the migration:

```ir,parse
(plan (insert migrations (values (columns a) (row 1)) (columns version)))
```

## Printing the schema

`Session::schema_text` returns the user schema as a script that recreates it on a new database. Statements come in dependency order: namespaces, principal types, tables with their indexes, entity types, memberships, foreign keys, then changed function properties.

```rust,ignore
let schema = session.schema_text().await?;
other_database_session.execute_script(&schema).await?;
```

- Objects in `public` are named without a namespace, so run the script with the default search path.
- The script holds no rows, policies or privileges.
- Printing is canonical: equal schemas print identically, so the text can be checked in and compared.
