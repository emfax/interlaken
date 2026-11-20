# Types

Every expression in a resolved plan has a type, and every type says whether the value may be NULL.

## Scalar types

| Type | Values | Literal | Notes |
| --- | --- | --- | --- |
| `bool` | true, false | `true`, `false` | |
| `int2` | 16-bit integers | `(int2 7)` | |
| `int4` | 32-bit integers | `(int4 7)` | |
| `int8` | 64-bit integers | `7` | A bare integer; see [implicit conversions](#implicit-conversions). |
| `float8` | 64-bit IEEE floats | `1.5`, `(float8 nan)`, `(float8 inf)`, `(float8 -inf)` | |
| `text` | UTF-8 strings | `"text"` | Compared by Unicode code point, so `"B" < "a"`. |
| `bytea` | byte strings | `(bytea "deadbeef")` | Hex digits, two per byte. |
| `date` | calendar dates | `(date "2026-01-31")` | Years may have up to nine digits or a leading `-`. |
| `timestamptz` | instants, to the microsecond | `(timestamptz "2026-01-31T09:30:00Z")` | `T` or a space before the time, optional fraction of up to six digits, then `Z` or an offset such as `+02:00`. Stored as UTC. |
| `uuid` | UUIDs | `(uuid "123e4567-e89b-12d3-a456-426614174000")` | |
| `jsonb` | JSON documents | `(cast "{\"k\": 1}" jsonb)` | Stored as text, without validation yet. Can't be compared, sorted or used as a key. |

`null` is a NULL of no particular type; its type comes from its context (see below).

## Nullability

In the type syntax, a type name alone is nullable. `(not-null T)` excludes NULL:

```ir,parse
(param 1 (not-null int8))
```

The resolver works out each expression's nullability:
- A column reference is nullable if the column is.
- Most operators return NULL if any operand is NULL, so they are nullable if any operand is.
- Some expressions are never NULL whatever their operands: `is-null`, `is-distinct-from`, `count` and `exists`.
- An outer join's nullable side makes its columns nullable.

Columns declared `(not-null ...)` reject NULLs when written, with `23502`. A table's primary key columns are always NOT NULL.

## Composite types

| Type | Syntax | Notes |
| --- | --- | --- |
| array | `(array T)` | Elements of type `T`. `(array text)` allows NULL elements; `(array (not-null text))` doesn't. Literal: `(array 1 2 3)`. |
| record | `(record (NAME T)...)` | Named fields. `nest` produces arrays of records; there is no record literal. |
| entity | `(entity NAME)` | A reference to an entity of type `NAME`, used by policies. |

Arrays, records and entities can't be compared, so they can't be sorted, grouped on, used with `distinct`, or combined by `union`, `intersect` or `except` (except `union :all true`).

**Known limitation.** An array literal's element type records whether any element is NULL. `(array "a" "b")` has type `(array (not-null text))`, and array types convert only to themselves. So that literal can't be written to an `(array text)` column, and `(array "a" null)` can't be written to an `(array (not-null text))` column. Declare array columns with the element nullability your data has. Until conversions between element nullabilities exist (backlog `array-element-nullability`), a non-null literal can't be stored in a column that also holds arrays with NULL elements.

## Implicit conversions

Operators need operands of the same type. Where operand types differ, the resolver converts them to a common type when no information is lost. These conversions apply to the operands of operators, `case`, `coalesce`, function arguments, set operations and the columns of `values`:

- `int2` to `int4` to `int8`;
- any integer to `float8`;
- `date` to `timestamptz`.

Each conversion becomes an explicit `cast` in the resolved plan.

Two literals are flexible:
- **A bare integer** takes the smallest integer type that holds it, so `(= x 5)` with an `int4` column `x` compares as `int4` rather than widening the column. In function calls, a bare integer prefers `int8`: `(call abs -5)` returns `int8`.
- **An untyped `null`** takes the type its context needs. With no context, as in a `values` column of only NULLs, it is `text`.

There is no implicit conversion between unrelated types. `(= "a" 1)` fails with `42804`:

```ir,error=42804
(plan (filter (get users :as u) (= u.name 1)))
```

## Explicit casts

`(cast EXPR TYPE)` converts a value. These casts are allowed:

| From | To | Notes |
| --- | --- | --- |
| any type | the same type | |
| any numeric | any numeric | Out-of-range values fail with `22003`. Floats round to the nearest integer, ties to even: 2.5 becomes 2. |
| integers | `bool` | 0 is false, anything else true. |
| `bool` | integers | false is 0, true 1. |
| any scalar | `text` | `bytea` becomes `\x` followed by hex; dates and timestamps use the literal formats above. |
| `text` | any scalar | Invalid text fails with `22P02`. |
| `date` | `timestamptz` | Midnight UTC. |
| `timestamptz` | `date` | The UTC date. |

Arrays, records and entities cast only to themselves.

Text is parsed after trimming spaces:

| Target | Accepted text |
| --- | --- |
| `bool` | `t`, `true`, `y`, `yes`, `on`, `1`, `f`, `false`, `n`, `no`, `off`, `0`, in any case |
| integers | decimal digits with an optional sign |
| `float8` | decimal or exponent notation, `nan`, `inf`, `infinity`, `-inf`, `-infinity` |
| `bytea` | `\x` followed by hex digits |
| `date`, `timestamptz`, `uuid` | the literal formats above |
| `jsonb` | anything (not validated yet) |

Postgres accepts more text formats, such as other date styles. Those are planned with the SQL front end (backlog `interp-text-formats`).

```ir
(plan (project (get users :as u) (cast u.id text) (cast "2026-03-04" date)))
```

## Assignment conversions

Values written by `insert` and `update` may also be converted where Postgres's assignment casts allow it, beyond the implicit conversions:

- any numeric to any numeric, failing with `22003` if the value doesn't fit;
- `timestamptz` to `date`;
- any scalar to `text`.

`text` doesn't convert to other types on assignment: cast it explicitly.

## What tables can hold

| Type | Column | Key column |
| --- | --- | --- |
| scalars other than `jsonb` | yes | yes |
| `jsonb` | yes | no |
| arrays of scalars | yes | no |
| arrays of arrays, records, entities | no | no |

Key columns are primary key and index columns. DDL that uses a type a table can't hold fails with `42P16`. See [Tables and column types](../ddl/tables.md).
