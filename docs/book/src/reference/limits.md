# Limits and unsupported features

Interlaken is pre-1.0. This chapter collects what it doesn't do yet, and the limits that apply to what it does. Items with a backlog name are tracked in `docs/backlog.md`.

## Not supported yet

### Interfaces

| Feature | Status |
| --- | --- |
| SQL | planned as a front end that lowers into the IR; until then, statements are [IR text](../ir/syntax.md) or plans built with the IR's Rust builder |
| A network protocol | none: Interlaken is embedded, and clients call the Rust API in-process |
| GraphQL | planned as a front end; the IR's `nest` and `unnest` operators and `apply` decorrelation already exist for it |
| Replication | single node only; replication with Raft is planned |
| User-defined functions | only built-in functions; WASM functions are designed (ADR-0029) but not built |

### Data definition

- **Columns:** a column can be added, but not dropped, renamed or retyped, and a table can't be renamed (see [Tables and column types](../ddl/tables.md)).
- **Defaults:** columns have no default values; an omitted column is NULL.
- **Constraints:** primary keys, unique indexes and NOT NULL only. There are no foreign keys, check constraints or sequences.
- **Views:** none.
- **Types tables can't hold:** records, entity references and arrays of arrays (`42P16`). Arrays and `jsonb` can't be key columns, and float columns are never used as index keys.

### The IR text language

- **Subquery positions:** subqueries aren't allowed in later `case` branches and `coalesce` arguments, in outer, semi and anti join conditions, or in `values`, `limit`, `offset` and `returning` (`0A000`; backlog `subquery-positions`). See [Where subqueries are allowed](../ir/subqueries.md#where-subqueries-are-allowed).
- **Text input formats:** casts from text accept ISO dates and timestamps, hex `bytea` and a basic set of booleans (backlog `interp-text-formats`).
- **`jsonb`** is stored as text without validation, and can't be compared, sorted or used as a key.
- **Array element nullability:** `(array (not-null T))` doesn't convert to `(array T)`, so an array literal with no NULLs can't be written to an array column that allows them (backlog `array-element-nullability`). See [Composite types](../ir/types.md#composite-types).

### Security

- **Undeclared principal types:** Cedar policies can only name declared entity types, so a principal like `Admin::"root"` can't be granted access in Cedar, and its queries on governed tables fail with `42501` under the default empty Cedar set (backlog `undeclared-principal-types`). See [Governed tables](../security/model.md#governed-tables).
- **Privileges are coarse:** `admin` covers all DDL and policy changes; there are no per-table or delegated privileges.
- **Masks on array columns** aren't supported (`42501`).
- **Impact analysis samples** above a million decisions, and the threshold isn't configurable (see [Changing policies](../security/proposals.md)).

### Execution

- **No spilling:** a blocking operator that exceeds its memory budget fails its query with `53200` (see [Memory](../operations/memory.md)).
- **No statistics or cost-based planning:** the planner chooses access paths and join algorithms by rules. There are no merge joins or streaming aggregations.
- **Large sorts don't yield:** the final sort of a large buffered input holds its thread until it finishes (backlog `sort-yields`; see [Cooperative scheduling](../transactions/timeouts.md#cooperative-scheduling)).

### Storage

- **On-disk format:** not stable between versions. A database from an incompatible version fails to open with a format version error.
- **Block cache:** fixed at fjall's default of 32 MiB.
- **Memtable size:** fixed when the database is created.

## Limits

| Limit | Default | Setting | When exceeded |
| --- | --- | --- | --- |
| a statement's memory for blocking operators | 256 MiB | `EngineOptions::query_budget` | the statement fails with `53200` |
| memory for blocking operators across all statements | none | `EngineOptions::node_memory_limit` | the statement that crosses it fails with `53200` |
| a transaction's buffered writes | 64 MiB of keys and values | `StorageOptions::max_write_set_bytes` | the write fails with `54000` |
| how long a transaction may stay open | 5 minutes | `StorageOptions::max_transaction_age` | its next operation fails with `25P04` |
| how long a statement may run | none | `Session::set_statement_timeout` | the statement fails with `57014` |

See [Configuration](../operations/configuration.md) for every setting.

Two further limits are behaviours rather than settings:

- **`Session::query` collects every row** of a result in memory, outside any budget. Use `query_stream` for large results (see [Streaming results](../transactions/streaming.md)).
- **DDL conflicts broadly:** a transaction that ran DDL conflicts with every concurrent transaction that wrote anything, whichever tables they touched, and one of them fails with `40001` (see [Tables and column types](../ddl/tables.md#transactions-and-concurrency)).
