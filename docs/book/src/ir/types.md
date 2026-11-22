# Types

Every expression in a resolved plan has a type, and every type says whether the value may be NULL: a type is either plain, and never NULL, or optional.

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

## Optional types

A type name alone is plain: an `int8` is always an integer, never NULL. `(option T)` is the type whose values are those of `T` or NULL:

```ir,parse
(param 1 (option int8))
```

Options don't nest: `(option (option int8))` is rejected.

The resolver works out which expressions are optional:
- A column reference is optional if the column's type is.
- Most operators return NULL if any operand is NULL, so they are optional if any operand is.
- Some expressions are never NULL whatever their operands: `is-null`, `is-distinct-from`, `count` and `exists`.
- An outer join pads its unmatched side with NULLs, so that side's columns become optional.

Columns of plain type reject NULLs when written, with `23502`. A table's primary key columns never hold NULL, whatever their declared type.

Resolved plans use two-valued logic: every predicate is a plain `bool`, and where a value may be absent the plan says so explicitly, with `lift`. You can still write SQL-style text, where NULL behaves as in SQL; the resolver rewrites it into the explicit form. See [NULL and three-valued logic](expressions.md#null-and-three-valued-logic).

## Composite types

| Type | Syntax | Notes |
| --- | --- | --- |
| array | `(array T)` | Elements of type `T`. `(array (option text))` allows NULL elements; `(array text)` doesn't. Literal: `(array 1 2 3)`. |
| record | `(record (NAME T)...)` | Named fields. `nest` produces arrays of records; there is no record literal. |
| entity | `(entity NAME)` | A reference to an entity of type `NAME`, used by policies. |

Arrays, records and entities can't be compared, so they can't be sorted, grouped on, used with `distinct`, or combined by `union`, `intersect` or `except` (except `union :all true`).

An array literal's type says whether any element is NULL: `(array "a" "b")` has type `(array text)`, and `(array "a" null)` has type `(array (option text))`. Arrays with plain elements convert implicitly to arrays with optional ones (see below), so both literals can be written to an `(array (option text))` column. The reverse isn't allowed: `(array "a" null)` can't be written to an `(array text)` column.

## Implicit conversions

Operators need operands of the same type. Where operand types differ, the resolver converts them to a common type when no information is lost. These conversions apply to the operands of operators, `case`, `coalesce`, function arguments, set operations and the columns of `values`:

- `int2` to `int4` to `int8`;
- any integer to `float8`;
- `date` to `timestamptz`;
- plain to optional inside arrays and records: `(array text)` to `(array (option text))`, and a record whose fields are plain to one whose fields are optional. Values don't change, so this conversion costs nothing at run time.

A plain value is always acceptable where an optional one is expected, with no conversion at all.

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

Arrays, records and entities cast only to themselves, and to the types their implicit conversions reach.

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
