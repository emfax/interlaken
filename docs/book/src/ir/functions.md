# Built-in functions

Scalar functions are called with `call`, and aggregates appear in an `aggregate` operator's `aggs` list:

```text
(call lower d.title)
(aggregate (get docs :as d) (keys d.folder) (aggs (agg count) (agg max d.updated)))
```

Each entry below lists the function's signatures and properties:

- **Signatures** use the type names of the IR text format. `T` stands for each of the listed types in turn, at every position it appears. A return type without `(not-null ...)` may be NULL.
- **Immutable** functions return the same result for the same arguments, always, so the optimizer may evaluate them once. **Stable** functions return the same result throughout a statement. **Volatile** functions may return a different result on every call.
- **Strict** functions return NULL when any argument is NULL, without being called. Strict aggregates skip rows whose argument is NULL.
- **Leakproof** functions reveal nothing about their arguments through errors or side effects, so the optimizer may evaluate them below a policy filter, on rows the principal can't see. No built-in function is leakproof yet; each needs an audit first (ADR-0011). Comparisons, `NOT`, `IS [NOT] NULL` and `IS [NOT] DISTINCT FROM` are leakproof operators.

This reference is generated from the engine's function catalog (`builtin_functions` and `builtin_description` in `interlaken-ir`), and a test fails if it falls out of date.

<!-- begin generated: functions -->
## Scalar functions

### `abs`

immutable · strict · not leakproof

- `abs(T) → T` for T: `float8`, `int2`, `int4`, `int8`

The absolute value; the minimum integer of its type overflows (`22003`).

### `assert_present`

immutable · not leakproof

- `assert_present(T) → (not-null T)` for T: any type

Its argument, or an error (`22004`) if it is NULL. Security lowering uses it for entity attributes a policy reads that may be absent.

### `gen_random_uuid`

volatile · not leakproof

- `gen_random_uuid() → (not-null uuid)`

A random version 4 UUID, from the same generator as `random`.

### `length`

immutable · strict · not leakproof

- `length(text) → int4`

The number of characters (not bytes) in the text.

### `lower`

immutable · strict · not leakproof

- `lower(text) → text`

The text with every character converted to lower case.

### `mask_last4`

immutable · strict · not leakproof

- `mask_last4(text) → text`

A column mask: every character but the last four replaced by `*`; text of four characters or fewer is unchanged.

### `now`

stable · not leakproof

- `now() → (not-null timestamptz)`

The time the current transaction started; the same for every call in it.

### `policy_mask`

volatile · not leakproof

- `policy_mask(bool, T, T) → T` for T: any type

`policy_mask(visible, value, masked)`: `value` where `visible` is true, else `masked`. Security lowering uses it for masked columns; it is volatile so the optimizer never moves the condition, and `EXPLAIN` hides it.

### `random`

volatile · not leakproof

- `random() → (not-null float8)`

A pseudo-random value in [0, 1), from a generator seeded when the transaction starts.

### `redact`

immutable · strict · not leakproof

- `redact(text) → text`

A column mask: always `****`.

### `substr`

immutable · strict · not leakproof

- `substr(text, int4, int4) → text`

`substr(t, start, count)`: the characters of `t` from 1-based position `start` for `count` characters; positions before the first character count towards `count`, and a negative `count` is an error (`22011`).

### `upper`

immutable · strict · not leakproof

- `upper(text) → text`

The text with every character converted to upper case.

## Aggregate functions

### `avg`

immutable · strict · not leakproof

- `avg(T) → float8` for T: `float8`, `int2`, `int4`, `int8`

The mean of the non-NULL values as `float8`, or NULL if there are none.

### `bool_and`

immutable · strict · not leakproof

- `bool_and(bool) → bool`

True if every non-NULL value is true; NULL if there are none.

### `bool_or`

immutable · strict · not leakproof

- `bool_or(bool) → bool`

True if any non-NULL value is true; NULL if there are none.

### `count`

- `count() → (not-null int8)` (immutable · not leakproof)
- `count(T) → (not-null int8)` for T: any type (immutable · strict · not leakproof)

With no argument (`count(*)`), the number of rows; with one, the number of rows whose argument is not NULL.

### `max`

immutable · strict · not leakproof

- `max(T) → T` for T: `bool`, `bytea`, `date`, `float8`, `int2`, `int4`, `int8`, `text`, `timestamptz`, `uuid`

The largest non-NULL value, or NULL if there are none.

### `min`

immutable · strict · not leakproof

- `min(T) → T` for T: `bool`, `bytea`, `date`, `float8`, `int2`, `int4`, `int8`, `text`, `timestamptz`, `uuid`

The smallest non-NULL value, or NULL if there are none.

### `sum`

immutable · strict · not leakproof

- `sum(int2) → int8`
- `sum(int4) → int8`
- `sum(int8) → int8`
- `sum(float8) → float8`

The sum of the non-NULL values, or NULL if there are none. Integer sums are exact and return `int8`, failing with `22003` if the total does not fit.

<!-- end generated: functions -->
