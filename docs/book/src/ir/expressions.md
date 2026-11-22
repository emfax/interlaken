# Literals and expressions

Expressions compute values: in filters, projections, join conditions, sort keys, aggregates, assignments and `RETURNING`. They follow Postgres semantics, including three-valued logic and NULL handling.

## Column references and literals

- **Column references** are names: `title`, `d.title`, or `#1.2` in resolved plans. See [Syntax and names](syntax.md#column-references).
- **Literals** are `true`, `false`, `null`, integers (`int8`), floats (`float8`) and strings (`text`), plus the typed forms `(int2 N)`, `(int4 N)`, `(float8 nan)`, `(bytea "hex")`, `(date "...")`, `(timestamptz "...")` and `(uuid "...")`. [Types](types.md#scalar-types) lists their formats.
- **Parameters** are `(param N TYPE)`, the value of the statement's `N`th parameter. See [Parameters and prepared plans](parameters.md).

## Operators

Operators are written prefix, with the operator name first: `(+ a b)`, `(= a b)`.

| Operator | Operands | Result |
| --- | --- | --- |
| `(= a b)`, `(<> a b)`, `(< a b)`, `(<= a b)`, `(> a b)`, `(>= a b)` | two values of one comparable type | `bool` |
| `(is-distinct-from a b)`, `(is-not-distinct-from a b)` | two values of one comparable type | `bool`, never NULL |
| `(+ a b)`, `(- a b)`, `(* a b)`, `(/ a b)` | two numerics of one type | that type |
| `(% a b)` | two integers of one type | that type |
| `(+ d n)`, `(- d n)` | a `date` and an `int4` | `date` |
| `(- d1 d2)` | two dates | `int4`, the days between them |
| `(concat a b)` | two `text` or two `bytea` | the same type |
| `(like s pattern)` | two `text` | `bool` |
| `(neg a)` | a numeric | its type |
| `(not a)` | `bool` | `bool` |
| `(is-null a)`, `(is-not-null a)` | any | `bool`, never NULL |
| `(and a ...)`, `(or a ...)` | any number of `bool` | `bool` |

[Implicit conversions](types.md#implicit-conversions) make operand types match where they can, so `(+ i4 i8)` adds as `int8`. A bare integer literal adapts to the other operand.

- **Arithmetic** follows Postgres:
  - Integer `/` truncates toward zero, so `(/ -7 2)` is -3, and `%` takes the sign of its first operand, so `(% -7 2)` is -1.
  - Overflow fails with `22003`, for floats as well as integers.
  - Division or `%` by zero fails with `22012`, for floats as well as integers.
  - `nan` and the infinities can be written as literals or produced by casts. As in Postgres, `nan` equals itself and sorts after every other value, including infinity.
- **Comparisons** of `text` use Unicode code point order: the C collation, not a locale's.
- **`like`** matches `%` to any run of characters and `_` to any one, and `\` escapes the next character. A pattern ending in `\` fails with `22025`.

## NULL and three-valued logic

You can write expressions as in SQL. A comparison or arithmetic operator with a NULL operand returns NULL, and so do most functions (those marked *strict* in [Built-in functions](functions.md)). `and` and `or` use three-valued logic:

| `a` | `b` | `(and a b)` | `(or a b)` |
| --- | --- | --- | --- |
| true | NULL | NULL | true |
| false | NULL | false | NULL |
| NULL | NULL | NULL | NULL |

`(not null)` is NULL. A filter keeps only rows where its predicate is true, so NULL drops a row just as false does.

To compare values that may be NULL, use `is-null`, or `is-distinct-from`, which treats NULL as equal to NULL and different from any value:

```ir
(plan (filter (get users :as u) (is-null u.dept)))
(plan (filter (get users :as u) (is-distinct-from u.dept "eng")))
```

The second keeps users whose department is anything but `"eng"`, NULL included. `(<> u.dept "eng")` would drop the NULLs.

### What resolved plans look like

Resolved plans, as `EXPLAIN` prints them, use two-valued logic instead ([ADR-0034](../reference/design-records.md)). The resolver rewrites SQL-style text into explicit forms that mean the same thing:

- **`(lift E)`** applies the operator or function call `E` to values that may be absent: the result is NULL if any operand is NULL, as above. A strict operator or function never takes an optional operand except inside `lift`, so the plan shows every place where NULL propagates. `(= u.dept "eng")` on an optional `dept` becomes `(lift (= u.dept "eng"))`, of type `(option bool)`.
- **`and3`, `or3` and `not3`** are the three-valued `and`, `or` and `not` of optional booleans. `and`, `or` and `not` themselves take only plain `bool`s.
- **Predicates are plain `bool`s.** A filter, join condition, `case` condition or aggregate filter that may be NULL is rewritten to say that NULL counts as false: `(coalesce (lift (= u.dept "eng")) false)`. The rewrite goes through `and3`, `or3` and `not3`, so a filter on `(and (= u.dept "eng") (> u.id 5))` becomes `(and (coalesce (lift (= u.dept "eng")) false) (> u.id 5))`, and indexes and pushdown work on each part as before.
- **Total equality**, where NULL equals NULL, is `is-not-distinct-from`. `=` on optional values is always lifted.

```ir
(plan (filter (get users :as u) (and (= u.dept "eng") (> u.id 5))))
```

You can write the explicit forms yourself; the resolver leaves them as they are, and `EXPLAIN` output can be pasted back in.

## Conditionals

**`case`** chooses between results. Each `(when CONDITION RESULT)` is tried in order, and the first whose condition is true gives the result. `(else RESULT)` gives the result when none matches. Without `else`, the result is NULL.

```ir
(plan (project (get docs :as d)
  (as (case (when (is-null d.title) "untitled") (when (= d.owner 1) "mine") (else d.title)) label)))
```

With `:on VALUE`, each `when` compares its first argument with `VALUE` for equality, as SQL's simple `CASE` does:

```ir
(plan (project (get users :as u)
  (case (when 1 "first") (when 2 "second") (else "other") :on u.id)))
```

**`coalesce`** returns its first non-NULL argument, or NULL if every argument is NULL. **`nullif`** returns NULL if its two arguments are equal, and the first otherwise:

```ir
(plan (project (get users :as u) (coalesce u.dept "none") (nullif u.dept "eng")))
```

The results of `case` and `coalesce`, like the operands of an operator, are converted to a common type.

## Casts, arrays and function calls

- **`(cast EXPR TYPE)`** converts a value; see [Explicit casts](types.md#explicit-casts).
- **`(array E...)`** builds an array of the elements' common type. The elements are NULL-free unless one of them may be NULL; see the [array limitation](types.md#composite-types).
- **`(call FUNCTION ARG...)`** calls a scalar function. The overload needing the fewest implicit conversions wins, and a tie fails with `42725`. Aggregates can't be called this way: they belong in an [`aggregate`](operators.md#aggregate) operator, and calling one elsewhere fails with `42000`. [Built-in functions](functions.md) lists every function.

```ir
(plan (project (get docs :as d) (call upper d.title) (array d.id d.owner) (cast d.updated date)))
```

## Subqueries

`(scalar REL)`, `(exists REL)`, `(in E REL)`, `(any OP E REL)` and `(all OP E REL)` are expressions over a relational subquery. [Subqueries](subqueries.md) describes them and where they are allowed.

## Errors at run time

Errors raised while evaluating an expression fail the whole statement and undo its writes (see [Statements, atomicity and savepoints](../transactions/statements.md)):

| Code | When |
| --- | --- |
| `22003` | arithmetic overflow, or a value that doesn't fit its target type |
| `22012` | division or modulo by zero |
| `22P02` | text that doesn't parse as the type it is cast to |
| `22011` | `substr` with a negative length |
| `22025` | a `like` pattern ending in `\` |
| `21000` | a scalar subquery returning more than one row |

```ir,error=22012
(plan (values (columns ratio) (row (/ 1 0))))
```
