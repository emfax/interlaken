# Indexes

Every table is stored in primary key order. A secondary index stores the table's rows in another key order, so queries that filter or join on other columns can read only the rows they need.

## Creating an index

```rust
# use interlaken_engine::catalog::{ColumnSpec, IndexKey, IndexSpec, TableSpec};
# use interlaken_engine::{Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::str("Admin", "root");
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));
# session.create_table(&TableSpec {
#     name: "docs".into(),
#     columns: vec![
#         ColumnSpec::new("id", parse_type("int8")?),
#         ColumnSpec::new("owner", parse_type("(option int8)")?),
#         ColumnSpec::new("updated", parse_type("(option timestamptz)")?),
#         ColumnSpec::new("slug", parse_type("(option text)")?),
#     ],
#     primary_key: vec!["id".into()],
# }).await?;
session
    .create_index(&IndexSpec {
        name: "docs_by_owner".into(),
        table: "docs".into(),
        columns: vec![IndexKey::asc("owner"), IndexKey::desc("updated")],
        unique: false,
        nones_distinct: true,
    })
    .await?;
session
    .create_index(&IndexSpec {
        name: "docs_slug".into(),
        table: "docs".into(),
        columns: vec![IndexKey::asc("slug")],
        unique: true,
        nones_distinct: true,
    })
    .await?;
# Ok(())
# }
```

In IR text, a key column is a bare name (ascending, NULLs last), or `(asc COLUMN)` or `(desc COLUMN)`, with `:nulls first` or `:nulls last` to change the NULL order:

```ir
(create-index docs_by_owner_text :table docs owner (desc updated))
(create-index docs_title :table docs title :unique true :nones-distinct false)
```

An `IndexSpec` has:

- **`name`:** unique across the database, not just the table. Names starting with `sys_` are reserved. A duplicate name fails with `42P07`.
- **`table`:** the indexed table.
- **`columns`:** one or more key columns, in key order. `IndexKey::asc` sorts ascending with NULLs last, and `IndexKey::desc` descending with NULLs first, as in Postgres. Both fields of `IndexKey` can be set directly for other combinations. Arrays and `jsonb` columns can't be indexed.
- **`unique`:** reject a row whose key duplicates another row's, with `23505`.
  - As in Postgres, keys containing a NULL never conflict: any number of rows may have a NULL `slug`.
- **`nones_distinct`:** for a unique index, `true` keeps that behaviour. `false` treats NULLs as equal to each other, as Postgres's `NULLS NOT DISTINCT` does, so a second row with a NULL `slug` fails with `23505`.
  - Creating a unique index over existing duplicates fails with `23505`.

Creating an index fills it from the table's existing rows in the same statement. Indexes are maintained by every later insert, update and delete. `drop_index(name)`, or `(drop-index NAME)`, removes one. Index DDL needs `admin` and changes the schema version, like all DDL (see [Tables and column types](tables.md#transactions-and-concurrency)).

## When the planner uses an index

The planner has no statistics yet, so it chooses access paths by rules, not costs.

**Scans.** For a filter on a table, the planner considers every key: the primary key and each secondary index. A key is usable when the filter has equalities on a leading run of its columns, optionally followed by one range on the next column. The comparisons must be with values that don't depend on the row: literals, parameters or the principal's attributes. The planner prefers:

1. a unique key pinned completely (at most one row);
2. otherwise the key with the longest run of equalities;
3. then a key with a range;
4. on a tie, the primary key, which needs no second lookup.

```ir
(plan (filter (get docs :as d) (and (= d.owner 2) (> d.updated (timestamptz "2026-01-01T00:00:00Z")))))
```

With `docs_by_owner`, this reads only owner 2's rows from 2026 onwards. `EXPLAIN` shows the choice as `TableScan docs primary key ...` or `IndexScan docs using docs_by_owner ...`. The filter is still applied above the scan, so the access path only has to narrow the rows read. Float columns are never used as keys, because `-0` and NaN make key equality differ from SQL equality.

**Joins.** A join whose equality condition pins a leading run of some key of the right table can run as an *index lookup join*. For each batch of left rows, it fetches just the matching right rows through that key. The operator first reads both inputs. If the left side ends within 131,072 rows, it looks those rows up. Otherwise it runs an ordinary hash join, so a lookup never loses badly to a hash join.

```ir
(plan (join inner (filter (get users :as u) (= u.dept "eng")) (get docs :as d) (= d.owner u.id)))
```

Policies still apply to rows fetched through an index, before the join sees them (see [The security model](../security/model.md)).

## Choosing indexes

- Index the columns your filters compare with constants and parameters, equality columns first and the range column last.
- Index the foreign key columns your joins use, such as `docs.owner` above, when the left side of the join is usually small.
- Each index makes every write to the table do more work, so don't index columns nothing filters or joins on.
