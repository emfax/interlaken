# Built-in functions

Scalar functions are called with `call`, and aggregates appear in an `aggregate` operator's `aggs` list:

```ir
(plan (project (get docs :as d) (call lower d.title)))
(plan (aggregate (get docs :as d) (keys d.folder) (aggs (agg count) (agg max d.updated))))
```

Each entry below lists the function's signatures and properties:

- **Signatures** use the type names of the IR text format. `T` stands for each of the listed types in turn, at every position it appears. Only an `(option ...)` return type may be NULL.
- **Immutable** functions return the same result for the same arguments, always, so the optimizer may evaluate them once. **Stable** functions return the same result throughout a statement. **Volatile** functions may return a different result on every call.
- **Strict** functions return NULL when any argument is NULL, without being called. Strict aggregates skip rows whose argument is NULL.
- **Leakproof** functions reveal nothing about their arguments through errors or side effects, so the optimizer may evaluate them below a policy filter, on rows the principal can't see. No built-in function is leakproof yet; each needs an audit first (ADR-0011). Comparisons, `NOT`, `IS [NOT] NULL` and `IS [NOT] DISTINCT FROM` are leakproof operators.

This reference is generated from the engine's function catalog (`builtin_functions` and `builtin_description` in `interlaken-ir`), and a test fails if it falls out of date.

<!-- begin generated: functions -->
## Scalar functions

### `abs`

immutable · strict · not leakproof

- `abs(T) → (option T)` for T: `(option float8)`, `(option int2)`, `(option int4)`, `(option int8)`

The absolute value; the minimum integer of its type overflows (`22003`).

### `and3`

immutable · leakproof

- `and3((option bool), (option bool)) → (option bool)`

SQL's three-valued AND of optional booleans: false if either is false, else NULL if either is NULL, else true. The resolver uses it for `and` over optional values.

### `assert_present`

immutable · not leakproof

- `assert_present(T) → T` for T: any type

Its argument, or an error (`22004`) if it is NULL. Security lowering uses it for entity attributes a policy reads that may be absent.

### `gen_random_uuid`

volatile · not leakproof

- `gen_random_uuid() → uuid`

A random version 4 UUID, from the same generator as `random`.

### `length`

immutable · strict · not leakproof

- `length((option text)) → (option int4)`

The number of characters (not bytes) in the text.

### `lower`

immutable · strict · not leakproof

- `lower((option text)) → (option text)`

The text with every character converted to lower case.

### `mask_last4`

immutable · strict · not leakproof

- `mask_last4((option text)) → (option text)`

A column mask: every character but the last four replaced by `*`; text of four characters or fewer is unchanged.

### `not3`

immutable · leakproof

- `not3((option bool)) → (option bool)`

SQL's three-valued NOT of an optional boolean: NULL for NULL. The resolver uses it for `not` of an optional value.

### `now`

stable · not leakproof

- `now() → timestamptz`

The time the current transaction started; the same for every call in it.

### `or3`

immutable · leakproof

- `or3((option bool), (option bool)) → (option bool)`

SQL's three-valued OR of optional booleans: true if either is true, else NULL if either is NULL, else false. The resolver uses it for `or` over optional values.

### `policy_mask`

volatile · not leakproof

- `policy_mask((option bool), T, T) → (option T)` for T: any type

`policy_mask(visible, value, masked)`: `value` where `visible` is true, else `masked`. Security lowering uses it for masked columns; it is volatile so the optimizer never moves the condition, and `EXPLAIN` hides it.

### `random`

volatile · not leakproof

- `random() → float8`

A pseudo-random value in [0, 1), from a generator seeded when the transaction starts.

### `redact`

immutable · strict · not leakproof

- `redact((option text)) → (option text)`

A column mask: always `****`.

### `substr`

immutable · strict · not leakproof

- `substr((option text), (option int4), (option int4)) → (option text)`

`substr(t, start, count)`: the characters of `t` from 1-based position `start` for `count` characters; positions before the first character count towards `count`, and a negative `count` is an error (`22011`).

### `upper`

immutable · strict · not leakproof

- `upper((option text)) → (option text)`

The text with every character converted to upper case.

## Aggregate functions

### `avg`

immutable · strict · not leakproof

- `avg(T) → (option float8)` for T: `(option float8)`, `(option int2)`, `(option int4)`, `(option int8)`

The mean of the non-NULL values as `float8`, or NULL if there are none.

### `bool_and`

immutable · strict · not leakproof

- `bool_and((option bool)) → (option bool)`

True if every non-NULL value is true; NULL if there are none.

### `bool_or`

immutable · strict · not leakproof

- `bool_or((option bool)) → (option bool)`

True if any non-NULL value is true; NULL if there are none.

### `count`

- `count() → int8` (immutable · not leakproof)
- `count(T) → int8` for T: any type (immutable · strict · not leakproof)

With no argument (`count(*)`), the number of rows; with one, the number of rows whose argument is not NULL.

### `max`

immutable · strict · not leakproof

- `max(T) → (option T)` for T: `(option bool)`, `(option bytea)`, `(option date)`, `(option float8)`, `(option int2)`, `(option int4)`, `(option int8)`, `(option text)`, `(option timestamptz)`, `(option uuid)`

The largest non-NULL value, or NULL if there are none.

### `min`

immutable · strict · not leakproof

- `min(T) → (option T)` for T: `(option bool)`, `(option bytea)`, `(option date)`, `(option float8)`, `(option int2)`, `(option int4)`, `(option int8)`, `(option text)`, `(option timestamptz)`, `(option uuid)`

The smallest non-NULL value, or NULL if there are none.

### `sum`

immutable · strict · not leakproof

- `sum((option int2)) → int8`
- `sum((option int4)) → int8`
- `sum((option int8)) → int8`
- `sum((option float8)) → float8`

The sum of the non-NULL values, or 0 if there are none, unlike SQL's NULL (ADR-0034). Integer sums are exact and return `int8`, failing with `22003` if the total does not fit.

<!-- end generated: functions -->
