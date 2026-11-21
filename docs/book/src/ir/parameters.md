# Parameters and prepared plans

## Parameters

`(param N TYPE)` stands for the statement's `N`th parameter, counting from 1. The value is passed separately from the plan text, so it is never parsed as IR, and one plan text serves for every value:

```ir
(plan (filter (get docs :as d) (and (= d.owner (param 1 (option int8))) (> d.updated (param 2 (option timestamptz))))))
```

Values are passed as `interlaken_ir::interp::Datum`s, in order, to `Session::query`, `Session::query_stream` or their `_plan` variants. The declared type says which `Datum` variant to pass:

| Type | `Datum` |
| --- | --- |
| `bool` | `Bool(bool)` |
| `int2`, `int4`, `int8` | `Int2(i16)`, `Int4(i32)`, `Int8(i64)` |
| `float8` | `Float8(f64)` |
| `text` | `Text(String)` |
| `bytea` | `Bytea(Vec<u8>)` |
| `date` | `Date(i32)`, days since 1970-01-01 |
| `timestamptz` | `Timestamptz(i64)`, microseconds since 1970-01-01 00:00:00 UTC |
| `uuid` | `Uuid([u8; 16])` |
| `jsonb` | `Jsonb(String)` |
| `(array T)` | `Array(Vec<Datum>)` |
| any optional type, `(option T)` | `Null` |

The declared type types the parameter where it is used, just as a column's type would. Implicit conversions apply: an `int4` parameter compared with an `int8` column is widened.

A parameter may be used more than once, and parameters may be used in any order.

### Checking values

Before a statement runs, the engine checks the values passed against the parameters the plan declares. A statement that fails a check doesn't run, and fails like any other statement, aborting an explicit transaction:

| Check | Fails with |
| --- | --- |
| exactly one value per parameter number, up to the highest the plan declares | `08P01`: "the statement has 2 parameters, but 1 value was supplied" |
| each value is the `Datum` variant of its declared type, and so are array elements and record fields | `42804`: "parameter 1 is declared int8, but the value supplied is Int4(1)" |
| `Null` only for an optional parameter | `22004`: "parameter 1 is declared int8, but NULL was supplied" |

There are no conversions between variants: pass `Datum::Int8` for an `int8` parameter, not `Datum::Int4`. A number that no parameter uses still needs a value, of any variant, to keep the numbering. `EXPLAIN ANALYZE` runs a statement without parameter values, so it fails with `08P01` for a plan that declares any.

## Prepared plans

`Session::prepare` resolves and verifies IR text once and returns the resolved plan. `Session::query_plan` and `Session::query_plan_stream` then run it with parameters, without parsing or resolving it again:

```rust
# use interlaken_engine::catalog::{ColumnSpec, TableSpec};
# use interlaken_engine::{Engine, EngineOptions};
# use interlaken_ir::interp::Datum;
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));
# session.create_table(&TableSpec {
#     name: "users".into(),
#     columns: vec![
#         ColumnSpec::new("id", parse_type("int8")?),
#         ColumnSpec::new("name", parse_type("text")?),
#     ],
#     primary_key: vec!["id".into()],
# }).await?;
# session.query(r#"(plan (insert users (values (columns id name) (row 1 "ada") (row 2 "bob")) (columns id name)))"#, Vec::new()).await?;
let by_id = session
    .prepare("(plan (project (filter (get users :as u) (= u.id (param 1 (option int8)))) u.name))")
    .await?;
for id in [1, 2] {
    let result = session.query_plan(&by_id, vec![Datum::Int8(id)]).await?;
    println!("{:?}", result.rows());
}
# Ok(())
# }
```

A prepared plan is pinned to the catalog's schema version, which its text shows as `:schema-version N`. Every DDL statement increments the version, after which the plan fails with `0A000` ("plan was resolved at schema version 2, but the catalog is at 3"). Prepare it again to pick up the new catalog. Resolving again matters: a dropped column or a changed type would otherwise make the plan wrong.

Policies aren't part of a prepared plan. Each run applies the policies active at that moment, for the session's principal, so a plan prepared by one principal can't be used to read another's rows.

`print_plan` turns a prepared plan into text that can be stored and run later with `Session::query`, under the same schema-version rule.
