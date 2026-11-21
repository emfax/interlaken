# Inserting, updating and deleting

`insert`, `update` and `delete` are relational operators. Each writes one table, the *target*, from the rows of its input, and outputs its `returning` rows. Each statement is atomic: if any row fails, none of the statement's writes are kept (see [Statements, atomicity and savepoints](../transactions/statements.md)).

## insert

```text
(insert TABLE INPUT (columns COLUMN...) [(returning ITEM...)] [:as ALIAS])
```

Inserts one row per input row. The input is any relation: `values` for literal rows, or a query:

```ir
(plan (insert users (values (columns id name) (row 4 "dee") (row 5 "eve")) (columns id name)))
(plan (insert folders (project (get users :as u) (+ u.id 1000) u.name u.id) (columns id name owner)))
```

- **Column mapping:** `(columns ...)` names the target columns the input's columns fill, in order, and the counts must match. `(columns)` with no names means every column of the table, in table order.
- **Defaults:** columns not named are NULL. There are no column defaults yet.
- **Required columns:** an insert that omits a column of plain type fails with `42000` before it runs.
- **Conversions:** the input's columns are converted to the target columns' types by [assignment conversions](types.md#assignment-conversions), so an `int4` value fills an `int8` column, and a number fills a `text` column as text.

## update

```text
(update TABLE INPUT (set COLUMN EXPR)... [(returning ITEM...)] [:as ALIAS])
```

Changes the target rows the input reads. The input must read the target table exactly once, through a `get` of it. It usually filters it, and may join other tables:

```ir
(plan (update users (filter (get users :as u) (= u.id 1)) (set dept "research")))
(plan (update docs (join inner (get docs :as d) (get folders :as f) (= d.folder f.id))
  (set title (concat f.name (concat ": " d.title)))))
```

- **What assignments see:** each `(set COLUMN EXPR)` assigns one column, and is evaluated over the input row. So `d.title` is the old value, and columns of joined tables are available. Unassigned columns keep their values.
- **Duplicate matches:** an update changes each target row at most once. If several input rows match the same target row, as with a join to several matches, the first one is used.
- **NULL into a plain column:** assigning a literal `null` to a column of plain type fails with `42000` before the statement runs. A NULL computed at run time fails with `23502`.
- **Errors in the input:** if the input doesn't read the target table, or reads it more than once, the plan fails with `42000`.

## delete

```text
(delete TABLE INPUT [(returning ITEM...)] [:as ALIAS])
```

Deletes the target rows the input reads. As for `update`, the input must read the target table exactly once:

```ir
(plan (delete docs (filter (get docs :as d) (< d.updated (timestamptz "2020-01-01T00:00:00Z")))))
(plan (delete users (join anti (get users :as u) (get docs :as d) (= d.owner u.id))))
```

The second deletes every user who owns no documents.

## returning

`(returning ITEM...)` lists the output columns, computed from each written row: the new row for `insert` and `update`, and the deleted row for `delete`. Items are expressions over the target's columns, qualified by the target's `:as` alias or by the table name. Without `returning`, the statement outputs no columns.

```ir
(plan (insert users (values (columns id name) (row 6 "fay")) (columns id name)
  (returning id (as (call upper name) shout))))
(plan (update users (filter (get users :as u) (= u.id 1)) (set name "ada lovelace")
  (returning u.id u.name) :as u))
```

In the second plan, `u.name` in `returning` is the new name. `returning` expressions can't contain subqueries. For a principal whose column policies mask a column, `returning` shows the masked value (see [Column policies and masks](../security/masks.md)).

## Results

A DML statement's result has one row per row it wrote, with the `returning` columns. Without `returning`, those rows have no columns, so `QueryResult::row_count` is the number of rows written:

```rust,ignore
let result = session
    .query("(plan (update users (get users :as u) (set dept \"eng\")))", Vec::new())
    .await?;
println!("updated {} users", result.row_count());
```

A DML statement read through `Session::query_stream` runs to its end even if the stream is dropped early, so its writes are never lost by stopping reading (see [Streaming results](../transactions/streaming.md)).

## Constraints

Every row written is checked against the table's constraints:

| Constraint | Violation |
| --- | --- |
| primary key | `23505` |
| unique index | `23505` |
| column of plain type | `23502` |
| membership hierarchy without cycles | `23514` |
| [foreign key](../ddl/foreign-keys.md) | `23503`, checked when the statement ends |

The checks see the statement's earlier writes. Shifting every key of a table by one, as in `(set id (+ u.id 1))`, can therefore collide with a row the statement hasn't moved yet, and fail with `23505`, as it can in Postgres.

## What a statement sees

DML reads its input as of the start of the statement. A statement never sees its own writes in its input, so an update that moves rows forward in the order it scans them changes each row once, and `(insert users (project (get users ...) ...))` copies each existing row once rather than looping. This is Halloween protection (ADR-0026). Later statements in the same transaction see the writes.

## Writes and authorization

For a principal governed by policies, a write may change only the rows that principal's policies let them see and write. Each new or changed row must pass the table's write checks, or the statement fails with `42501` (see [Writes and write checks](../security/writes.md)). Writing a membership edges table also updates its closure table in the same statement, so later statements are authorized with the new memberships (see [Entities and memberships](../ddl/entities.md)).
