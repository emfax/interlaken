# Subqueries

A subquery is a relation used inside an expression. It may be *correlated*, referring to columns of the row the enclosing expression is evaluated for.

| Form | Value |
| --- | --- |
| `(scalar REL)` | The single column of the subquery's single row; NULL if it has no rows. More than one row fails with `21000`. |
| `(exists REL)` | Whether the subquery has any rows. Never NULL. |
| `(in E REL)` | Whether `E` equals a value of the subquery's single column. |
| `(any OP E REL)` | Whether `(OP E v)` is true for some value `v` of the subquery's single column. `OP` is a comparison: `=`, `<>`, `<`, `<=`, `>` or `>=`. |
| `(all OP E REL)` | Whether `(OP E v)` is true for every value `v`; true for an empty subquery. |

`in`, `any` and `all` use three-valued logic, as in SQL:
- **`in` and `any`** are true if some comparison is true, and false for an empty subquery, even when `E` is NULL. Otherwise they are NULL if `E` is NULL or the subquery holds a NULL, and false if not.
- **`all`** is true for an empty subquery and false if some comparison is false. Otherwise it is NULL if any comparison is NULL, and true if not.

So `(not (in E REL))` is never true when the subquery holds a NULL, a classic SQL surprise.

## Examples

Users who own at least one folder, then users who own none:

```ir
(plan (filter (get users :as u) (exists (filter (get folders :as f) (= f.owner u.id)))))
(plan (filter (get users :as u) (not (exists (filter (get folders :as f) (= f.owner u.id))))))
```

The same with `in`:

```ir
(plan (filter (get users :as u) (in u.id (project (get folders :as f) f.owner))))
```

Each user's folder name, or NULL. The subquery must return at most one row per user:

```ir
(plan (project (get users :as u) u.name
  (as (scalar (project (filter (get folders :as f) (= f.owner u.id)) f.name)) folder)))
```

Documents updated after every document in folder 10:

```ir
(plan (filter (get docs :as d)
  (all > d.updated (project (filter (get docs :as e) (= e.folder 10)) e.updated))))
```

## Correlation

Inside a subquery, an unqualified column name is looked up in the subquery's own input first, then in the enclosing query. A qualified name such as `u.id` matches the enclosing `get` aliased `u` (see [Column references](syntax.md#column-references)). Give the inner and outer tables different aliases to keep references unambiguous.

## Where subqueries are allowed

Subqueries may appear in the expressions of `filter`, `project`, `aggregate` keys and arguments, `sort` keys, `update` assignments, `nest` and `unnest`, and inner join conditions. These positions fail with `0A000` instead:

- **`case` and `coalesce`:** the branches of a `case` after its first condition, and the arguments of a `coalesce` after its first. SQL evaluates these only when they are reached. Interlaken evaluates subqueries eagerly for every row, which could raise an error, such as `21000`, for a row whose branch isn't taken. The first `when` condition and the first `coalesce` argument are always evaluated, so they are allowed.
- **Outer join conditions:** the conditions of `left`, `right`, `full`, `semi` and `anti` joins.
- **No row to evaluate them for:** `values` rows, `limit` and `offset`, and `returning`.

```ir,error=0A000
(plan (join left (get users :as u) (get folders :as f) (exists (filter (get docs :as d) (= d.folder f.id)))))
```

Most of these can be rewritten. A `case` branch subquery can usually move into an `apply` whose result the `case` then uses. An outer join's subquery condition can move into a filter on the join's input. Lifting the restrictions is backlog item `subquery-positions`.

## How subqueries run

The resolver lowers every subquery to an [`apply`](operators.md#apply), and the optimizer then turns most applies into joins:

- **Top-level conjuncts of a filter:** `exists`, `(not (exists ...))`, `in` and `any` become semi or anti joins.
- **Elsewhere:** a scalar subquery becomes a `single` apply, `exists` a `left` apply, and `in`, `any` and `all` a `single` apply over an aggregate that computes the three-valued result.
- **Decorrelation:** the optimizer then turns correlated applies into joins where it can. `(not (in ...))` in a filter becomes a null-aware anti join, so it runs as one pass over each side rather than a nested loop (ADR-0033).

`EXPLAIN` shows the plan that results.
