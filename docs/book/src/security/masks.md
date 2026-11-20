# Column policies and masks

Row policies decide which rows a principal sees. *Column policies* decide, row by row, whether a principal may see one column of a row they can see. Where they say no, the column is either denied outright or shows a *masked* value.

## Column policies

A column policy is a policy on the `read` action that names a column (an attribute of the entity type):

- in RLS, with `:column NAME`;
- in Cedar, with the annotation `@column("NAME")`.

For each row a principal can read, a column with column policies is visible if one of its column permits holds and none of its column forbids does. A column that no column policy names is visible on every row the principal can read.

```rls
(policies
  (permit (read) Doc true)
  (permit (read) Doc (== resource.owner principal) :column title :mask redact))
```

Everyone reads every document. Each sees the titles of their own documents, and `****` in place of everyone else's.

## Masks

What a principal gets where column policies deny depends on the mask:

| Column policies | Without a mask | With a mask |
| --- | --- | --- |
| allow every row the principal can read | the column is visible | the column is visible |
| allow some rows, depending on the row | reading the column fails with `42501` | denied rows show the mask |
| allow no row | reading the column fails with `42501` | every row shows the mask |

Without a mask, a principal can still run queries that don't read the column; only queries that do read it fail. The available masks are:

| Mask | RLS | Cedar | Shows |
| --- | --- | --- | --- |
| NULL | `:mask null` | `@mask("null")` | NULL |
| `redact` | `:mask redact` | `@mask("redact")` | `****` |
| `mask_last4` | `:mask mask_last4` | `@mask("mask_last4")` | every character but the last four replaced by `*` |
| any function | `:mask NAME` | `@mask("NAME")` | the function applied to the real value |

A masking function is any scalar function of the column's type that returns that type. Conflicting masks on one column are an error. NULL works on any column. Masks on array columns aren't supported, and fail with `42501`.

```rls
(policies
  (permit (read) User true)
  (permit (read) User (== resource principal) :column dept :mask null))
```

Every user can see every user, but only their own department; others' departments read as NULL.

## Where masks apply

A masked column holds the masked value everywhere the query reads it, not only in the output. Filters, join conditions, sort keys, groups and aggregates all see the masked value:

- A query filtering on `(= d.title "budget")` finds no rows whose title the principal sees masked, because they see `****`.
- `count` and `max` over a masked column count and compare the masked values.
- A principal can't learn a masked value by filtering, joining or sorting on it.

Writes read through masks as well, but never write a masked value over a real one:

- **Untouched columns:** an update or delete that doesn't reference a masked column behaves exactly as it would without masks. Columns the statement doesn't assign keep their real values.
- **Referenced columns:** where a statement's filter, join or assignment does reference a masked column of the table it writes, it sees the masked value, as a query would. The engine still writes the real row back: it re-reads the row by primary key, unmasked, behind the principal's back.
- **Assignments:** an assignment from a masked column copies the masked value. `(set folder secret)` writes NULL where `secret` is masked as NULL. That reveals nothing the principal can't already see.
- **`returning`:** `returning` shows masked values, as a later read of the row would.
- **Refused:** two statements fail with `42501`, because the engine couldn't carry the row unmasked:
  - one that reads a masked primary key column of the table it writes;
  - one that consumes the table's rows by position, as `distinct` over them does.

## EXPLAIN

For principals without the `read-policies` privilege, `EXPLAIN` shows each mask as `<policy> mask`, hiding its condition and function (see [EXPLAIN and policy visibility](explain.md)).
