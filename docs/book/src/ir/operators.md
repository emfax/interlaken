# Relational operators

A plan is a tree of relational operators. Each operator produces rows with named, typed columns. Most take one or two input operators as their first arguments.

| Operator | SQL equivalent |
| --- | --- |
| `get` | `FROM table` |
| `filter` | `WHERE`, `HAVING` |
| `project` | the `SELECT` list |
| `join` | `JOIN ... ON` |
| `apply` | `LATERAL` |
| `aggregate` | `GROUP BY` with aggregates |
| `distinct` | `SELECT DISTINCT` |
| `sort` | `ORDER BY` |
| `limit` | `LIMIT` / `OFFSET` |
| `union`, `intersect`, `except` | the set operations |
| `values` | `VALUES` |
| `nest`, `unnest` | `array_agg` of rows, `unnest` |

Rows have no order unless a `sort` gives them one. A client reads rows in the order of a `sort` at the top of the plan. A `filter`, `project` or `limit` above that `sort` keeps its order; other operators don't promise to.

## get

```text
(get TABLE [:as ALIAS])
```

Reads every row of a table, with all its columns. `:as` names the qualifier for its columns; without it, the qualifier is the table name. See [Tables and aliases](syntax.md#tables-and-aliases).

Policies apply here. For a principal whose policies restrict the table, `get` returns only the rows and columns they allow (see [The security model](../security/model.md)).

## filter

```text
(filter INPUT PREDICATE)
```

Keeps the input rows where `PREDICATE` is true; false and NULL drop the row. The predicate must be `bool`, or the plan fails with `42804`. Filters over aggregates do what SQL's `HAVING` does:

```ir
(plan (filter (aggregate (get docs :as d) (keys d.owner) (aggs (agg count))) (> count 1)))
```

## project

```text
(project INPUT ITEM...)
ITEM: EXPR | (as EXPR NAME)
```

Computes one output row per input row, with one column per item. Output columns are named as described in [Output column names](syntax.md#output-column-names).

```ir
(plan (project (get docs :as d) d.id (as (call upper d.title) title)))
```

## join

```text
(join KIND LEFT RIGHT CONDITION)
```

Combines the rows of two inputs where `CONDITION` is true. The condition can refer to columns of both inputs. To join without a condition, write `true`.

| Kind | Output rows | Output columns |
| --- | --- | --- |
| `inner` | each matching pair | left's, then right's |
| `left` | each matching pair, plus each left row with no match, padded with NULLs | left's, then right's, now nullable |
| `right` | each matching pair, plus each right row with no match | left's, now nullable, then right's |
| `full` | each matching pair, plus unmatched rows of both sides | both sides', all nullable |
| `semi` | each left row with at least one match, once | left's only |
| `anti` | each left row with no match | left's only |

```ir
(plan (project (join left (get users :as u) (get folders :as f) (= f.owner u.id)) u.name (as f.name folder)))
(plan (join anti (get users :as u) (get docs :as d) (= d.owner u.id)))
```

The second plan returns the users who own no documents.

## apply

```text
(apply KIND OUTER INNER)
```

Runs `INNER` once for each row of `OUTER`. `INNER` can refer to the outer row's columns, as SQL's `LATERAL` can. Subqueries in expressions are lowered to `apply` (see [Subqueries](subqueries.md)), but it can also be written directly.

| Kind | For each outer row |
| --- | --- |
| `inner` | the outer row joined to each inner row |
| `left` | as `inner`, or the outer row padded with NULLs if the inner side has no rows |
| `semi` | the outer row once, if the inner side has rows |
| `anti` | the outer row, if the inner side has no rows |
| `single` | the outer row joined to the one inner row, or padded with NULLs if there are none; more than one inner row fails with `21000` |

This plan returns each user with their most recently updated document, or NULLs:

```ir
(plan (apply left (get users :as u)
  (limit (sort (filter (get docs :as d) (= d.owner u.id)) (desc d.updated)) :count 1)))
```

## aggregate

```text
(aggregate INPUT (keys ITEM...) (aggs AGG...))
AGG: (agg FUNCTION ARG... [:distinct true] [:filter PREDICATE]) | (as (agg ...) NAME)
```

Groups the input rows by the key items and computes the aggregates over each group. The output has one row per group, with the keys' columns followed by the aggregates' columns:

- **Keys** must be comparable types (not `jsonb`, arrays or records).
- **With no keys**, the whole input is one group, and the output is exactly one row, even when the input is empty: `count` gives 0 and the other aggregates give NULL.
- **`:distinct true`** aggregates each distinct argument value once.
- **`:filter`** feeds the aggregate only the rows where its predicate is true, as SQL's `FILTER (WHERE ...)` does.

```ir
(plan (aggregate (get docs :as d) (keys d.owner)
  (aggs (agg count)
        (as (agg max d.updated) latest)
        (as (agg count :distinct true d.folder) folders)
        (as (agg count :filter (is-null d.title)) untitled))))
```

The aggregate functions are `count`, `sum`, `avg`, `min`, `max`, `bool_and` and `bool_or`; see [Built-in functions](functions.md#aggregate-functions).

## distinct

```text
(distinct INPUT)
```

Removes duplicate rows. NULLs count as equal to each other. Every column must be comparable.

## sort

```text
(sort INPUT KEY...)
KEY: (asc EXPR [:nulls first|last]) | (desc EXPR [:nulls first|last])
```

Orders the rows by each key in turn. By default NULLs sort last in ascending order and first in descending order, as in Postgres; `:nulls` overrides that. Sort keys must be comparable. The order of rows whose keys are all equal is unspecified.

```ir
(plan (sort (get docs :as d) (desc d.updated :nulls last) (asc d.id)))
```

## limit

```text
(limit INPUT [:count EXPR] [:offset EXPR])
```

Skips the first `:offset` rows, then returns at most `:count`. Both are `int8` expressions that can't refer to columns: write literals or parameters. A NULL count means no limit. A negative count fails with `2201W`, and a negative offset with `2201X`. Without a `sort` beneath it, which rows a `limit` returns is unspecified.

```ir
(plan (limit (sort (get users :as u) (asc u.id)) :count 10 :offset (param 1 int8)))
```

## union, intersect and except

```text
(union LEFT RIGHT [:all true])
(intersect LEFT RIGHT [:all true])
(except LEFT RIGHT [:all true])
```

The inputs must have the same number of columns. Corresponding columns are converted to their common type, and the output takes the left input's column names.

- **Without `:all`**, the result is a set: `union` returns the distinct rows of either input, `intersect` those in both, and `except` those in the left but not the right.
- **With `:all true`**, the result keeps duplicates. `union` returns every row of both inputs. `intersect` and `except` match rows one for one: a row appearing three times on the left and once on the right appears once in `intersect` and twice in `except`.

```ir
(plan (except (project (get users :as u) u.id) (project (get docs :as d) d.owner)))
```

## values

```text
(values (columns NAME...) (row EXPR...)...)
```

Produces the given rows. Each row must have one expression per column, and each column's expressions are converted to their common type; a column of only NULLs is `text`. `values` needs at least one row, and its expressions can't contain subqueries.

```ir
(plan (values (columns id label) (row 1 "one") (row 2 null)))
```

## nest

```text
(nest INPUT (keys ITEM...) (fields ITEM...) :as NAME)
```

Groups the input by the keys, as `aggregate` does, and collects each group's rows into one array column `NAME`. Each element is a record with one field per field item, and every field needs a name: a column reference or `as`. With no keys, the output is one row holding every input row, or an empty array for an empty input. Nested results are how the GraphQL front end will build its responses (ADR-0012).

```ir
(plan (nest (get docs :as d) (keys d.folder) (fields d.id d.title) :as items))
```

The output has columns `folder` and `items`, of type `(not-null (array (not-null (record (id (not-null int8)) (title text)))))`. The order of elements within an array is unspecified.

## unnest

```text
(unnest INPUT ARRAY_EXPR (columns NAME...))
```

Expands an array into rows. For each input row, it emits one row per element of `ARRAY_EXPR`, holding the input row's columns plus the element. An input row whose array is NULL or empty emits no rows:

- **A scalar element** becomes one column, named by `(columns NAME)`. With `(columns)` it is named `unnest`.
- **A record element** becomes one column per field. The columns take the field names, unless `columns` renames them.

```ir
(plan (project (unnest (get docs :as d) d.tags (columns tag)) d.id tag))
```

## Inserting, updating and deleting

`insert`, `update` and `delete` are relational operators too: their input is a relation, and their output is the `RETURNING` rows. They are described in [Inserting, updating and deleting](dml.md).
