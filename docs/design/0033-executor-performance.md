---
id: 0033
title: Executor Performance
status: implemented  # proposal | accepted | implemented | superseded
tags: [executor, storage, optimizer, memory, benchmarks]
feature-flag:
created: 2026-11-20
accepted: 2026-11-20
implemented: 2026-11-20
spec-sections: ["mvp-completion.md#f3-executor-performance", "executor-design.md#execution-model", "executor-design.md#memory-management"]
superseded-by:
---

# ADR-0033: Executor Performance

## Status

Implemented

## Summary

Milestone F3 measures the executor and fixes what the measurements show:
- **Benchmarks** over 1–10 million rows, through the engine's API, recording throughput and peak memory per operator family. A 10-million-row test checks the executor's memory criterion on demand.
- **Incremental aggregates:** every built-in aggregate except `DISTINCT` ones folds rows into a fixed-size state, exactly as the interpreter computes it, so aggregation memory follows the groups, not the rows.
- **Index lookup joins:** a join whose right input is a keyed table fetches the right rows for a whole left batch per storage call, and turns into a hash join when its left input turns out not to be small.
- **Batched row writes:** DML writes each input batch with one row-layer call, which reads all of the batch's constraint-check keys at once and otherwise behaves exactly like writing its rows one at a time.
- **Set-oriented quantified comparisons:** `IN`, `NOT IN`, `ANY` and `ALL` decorrelate into joins with aggregates of the subquery (a null-aware anti join for `NOT IN`).
- **Cooperative scheduling:** blocking operators yield to the runtime every few batches.
- **Decode stays on executor threads** (ADR-0023 phase 3 is not needed yet), and the default memory budgets are set from the measured peaks (ADR-0027).

## Context

E1–E5 built every operator for correctness; F1 put them behind the engine's API. Nothing had measured them. Several known gaps waited for numbers (`mvp-completion.md`, F3):
- Aggregates other than `count` kept every argument tuple, so memory grew with the input (backlog `incremental-aggregates`).
- An index nested loop ran as an `Apply`, one index scan per outer row, and a decorrelated equi-join on an indexed key read and hashed the whole indexed table (`batched-index-lookup`).
- DML called the row layer once per row, each call making its own constraint-check reads (`batched-row-writes`).
- `NOT IN` and `ALL` stayed nested loops (`not-in-decorrelation`).
- Operators never yielded, so F1 checked `statement_timeout` per batch.
- The memory budgets were placeholders (`default-memory-budgets`).

## Decision

### Benchmarks

`crates/engine/benches/executor.rs` (`cargo bench -p interlaken-engine --bench executor`) builds one database per run through engine DDL and loads it directly through storage: `t` with `BENCH_ROWS` rows (default 1,000,000), `dim` with a tenth of that, `small` (10,000 rows), and `docs` (as many as `t`) governed by a row-level policy that lets user 1 read 1%. Each case runs a statement `BENCH_RUNS` times through a session and reports the best and median wall time, throughput, the most query memory reserved at once (from the engine's node pool, which gained `MemoryPool::peak`), and the growth of the process's peak resident set (Linux, reset per run through `/proc/self/clear_refs`). The cases:
- scans: `count(*)`, a leakproof filter pushed into storage, a filter the executor evaluates;
- a hash join against `dim`;
- aggregates: scalar, 1,000 groups, 100,000 groups, float sums and averages;
- a full sort and a top-N;
- index nested loops: 10,000, 100,000 and all left rows against `t`'s primary key, and 10,000 against a unique secondary index;
- `NOT IN`: 1,000 outer rows and every row against `small`;
- secured queries: a scan and a join as user 1;
- bulk DML: insert, update and delete of a million rows.

`crates/exec/tests/ten_million.rs` is the executor's 10-million-row criterion, ignored by default (`cargo test --release -p interlaken-exec --test ten_million -- --ignored --nocapture`).

### Incremental aggregates

`count`, `sum`, `avg`, `min`, `max`, `bool_and` and `bool_or` without `DISTINCT` and with one argument accumulate into a fixed-size state per group (`operators/accumulate.rs`). Each fold is exactly the interpreter's `aggregate_values` over the same rows in the same order:
- Integer `sum` and `avg` add in `i128`; a sum outside `int8` fails when the group finishes.
- Float `sum` and `avg` add left to right starting from the empty sum, which is how Rust's `Iterator::sum`, and so the interpreter, folds. The results are bit-identical, including negative zero, infinities and NaN, so float sums need no tuples.
- `min` and `max` keep the first of equal values, with the interpreter's comparison. Text is compared in place, so losing values are never copied.
- All are strict: NULL arguments are skipped, and a group with no other row finishes as NULL (`count` as 0).

`DISTINCT` aggregates still keep their argument tuples, as do aggregates the executor doesn't know.

### Index lookup joins

The physical planner turns a join into `Op::IndexLookup` when:
- it is inner, left, semi or anti;
- its right input is a table scan under filters and policy filters;
- some of its equality keys compare a left expression with a right column of a keyable type (not float or `jsonb`) and pin a leading run of the primary key or a secondary index.

It prefers a key pinned completely and uniquely, then the longest run, then the primary key. For each left batch the operator evaluates the pinning keys and fetches the right rows for the distinct ones:
- **The whole primary key:** `get_many`.
- **A primary key prefix or a secondary index:** one prefix scan per distinct key, all in one call of the new `Txn::scan_many` (`rows::scan_many`, `rows::scan_index_many`). Prefixes with more rows continue in further calls. An index's rows then come from one `get_many`.

Calls carry at most 256 keys each and run concurrently, so a 1,024-row batch makes four parallel calls into the storage pool; that measured 1.5 times faster than one call per batch. The fetched rows go through the right input's conditions in plan order (the scan's policy and pushed conjuncts, then each filter and policy filter), so policy filters still run before anything the join evaluates. From there the operator joins them like a hash join: every equality key must match, NULL keys never match, and the residual runs on key-matching pairs (the shared `join::pair_up`).

**Lookup or hash join, decided at run time.** A lookup costs about 4.7 µs per left row on the benchmark machine, against about 1.7 µs per right row to build a hash table, and the planner has no statistics to compare the inputs. So the operator first reads both inputs, the right one twice as fast:
- **The left input ends first:** the right table has more than two rows per left row, so lookups win. It looks the left rows up and drops what it read of the right.
- **The right input ends first, or both pass 131,072 left rows:** it becomes an ordinary hash join, reusing every row already read.

The cost of deciding is reading at most twice the left rows of the right input. A join against a small table (such as the benchmark's secured join against 100 users) stays a hash join. Rows held while deciding count against the query budget as `IndexLookup`.

### Batched row writes

`rows::insert_many`, `update_many` and `delete_many` (`storage/src/rows/batch.rs`) behave exactly like calling `insert`, `update` and `delete` once per row, in order: the same results, the same error for the first failing row, the same rows and index entries. They read in bulk:
- **Insert:** one `get_many` for every row key and exclusive unique-index entry of the batch.
- **Delete:** one for the old rows.
- **Update:** one for the old rows, then one for the keys their new values must not collide with.

Rows are then written one at a time against an overlay of what the batch has written so far, so two rows of one batch with the same key still collide. A key that only an earlier row's write made relevant is read on demand. DML makes one call per input batch; a row that fails to convert still fails only after the rows before it were written, so errors match row-at-a-time writing too.

### Quantified comparisons

The resolver lowers `x op ANY (s)`, and through it `IN`, `NOT IN` and `ALL`, to a single apply over `bool_or(x op c)`, `count(*)` and `count(x op c)` over `s`, whose arguments read the outer row. A new decorrelation rule (`ir/src/optimize/quantified.rs`) computes the same three values from aggregates of `s` that don't read the outer row:
- `rows = count(*)`;
- `known` is 0 when `x` is NULL, otherwise `nn = count(c)`;
- `any_true` is NULL when `x` is NULL or `nn = 0`, and otherwise:
  - for `=`, whether `x` is among the distinct non-NULL `c`, found by a left join;
  - for `<`/`<=`, `x op max(c)`; for `>`/`>=`, `x op min(c)`;
  - for `<>`, `x <> min(c) or x <> max(c)`.

The aggregates join the outer side on `true`, or, for a subquery correlated by equalities, grouped by its inner expressions and joined on them, as the scalar-aggregate rule does. The rule requires `x` and `c` to be leakproof and the subquery to be free of volatile functions, since it reads the subquery twice under fresh bindings. Under `NOT` in a filter, the `=` form is a null-aware anti join.

### Cooperative scheduling

Hash join builds, sorts, aggregations, `Nest` and set operations yield to the runtime every 4 input batches (`coop::Yielder`), by waking their task and returning `Pending` once, which Tokio treats as a yield. The per-batch `statement_timeout` check F1 added stays.

### Results

One million rows, best of five runs (three for DML), on the 22-core development machine shared with other builds, before and after F3. Reserved is the most query memory held at once.

| Case | Before | After | Reserved before → after |
| --- | --- | --- | --- |
| `agg_scalar` (`sum`, `max`, `min`, `bool_or`) | 1.18 s | 0.78 s | 226 → 0 MiB |
| `agg_few_groups` (1,000 groups, five aggregates) | 2.24 s | 0.88 s | 280 → 0.7 MiB |
| `agg_float_sum` (float `sum` and `avg`, 1,000 groups) | 1.12 s | 0.85 s | 107 → 0.3 MiB |
| `agg_many_groups` (100,000 groups) | 0.95 s | 0.97 s | 13.7 → 13.7 MiB |
| `index_nl_pk` (10,000 lookups by primary key) | 1.45 s | 0.054 s | 132 → 1.9 MiB |
| `index_nl_secondary` (10,000 by a unique index) | 1.43 s | 0.073 s | 132 → 1.9 MiB |
| `index_nl_pk_wide` (100,000 lookups) | 1.94 s | 0.58 s | 132 → 18.6 MiB |
| `index_nl_pk_all` (a million: becomes a hash join) | 2.91 s | 2.89 s | 132 → 132 MiB |
| `not_in` (1,000 rows against 10,000) | 11.2 s | 0.024 s | 0.5 → 1.5 MiB |
| `not_in_large` (a million against 10,000) | over 300 s (failed at the transaction age limit) | 1.13 s | — → 1.5 MiB |
| `dml_insert` (a million rows) | 17.0 s | 6.4 s | 0 → 0 |
| `dml_update` | 21.2 s | 10.3 s | 69 → 69 MiB |
| `dml_delete` | 72.6 s | 29.4 s | 69 → 69 MiB |
| `secured_join` (100 users: stays a hash join) | 0.46 s | 0.47 s | 0 → 0 |

Scans, the hash join against `dim`, sorts, top-N and the secured scan are unchanged within noise: in a back-to-back rerun, `count(*)` went from 748 to 706 ms, the pushed-down scan from 588 to 579 ms, the hash join from 1.11 to 1.20 s, the sort from 1.63 to 1.55 s and top-N from 0.89 to 0.96 s. At ten million rows (`ten_million.rs`), a filter and projection streams 5 million rows out in 6.7 s with nothing reserved and 0.3 MiB of resident growth (once storage's block cache is warm), an aggregation into 1,000 groups takes 7.8 s and reserves 0.6 MB, and a full sort fails at the 256 MiB budget after 4.2 s with 205 MiB of resident growth.

### Decode on the storage thread (ADR-0023 phase 3)

Not built. A profile of the million-row scan benchmarks put 89% of CPU samples on storage pool threads (fjall iteration and MVCC version resolution) and 11% on executor threads, where row decoding (`decode_row`, `from_stored`, `build_array` and their allocations) was about 6% of the total. A scan waits on its storage calls, so moving decode onto the storage thread would lengthen exactly the calls it waits on, and rows a pushed filter rejects already never leave the storage pool. It stays a backlog item (`batch-decoder`) until executor threads saturate on decode.

### Default memory budgets

See ADR-0027 for the figures. The per-query default stays 256 MiB. The recommended node pool is a quarter of the memory left after storage's caches, because resident memory runs up to 2.6 times ahead of reservations for hash tables and about 4 times for a finishing sort.

## Implementation Phases

- [x] **Phase 1: Benchmarks, incremental aggregates, index lookup joins, batched writes, quantified comparisons, cooperative scheduling, budgets** - F3
  - *As implemented:* as described above.
  - *Testing:*
    - A new differential test runs 800 random plans over tables of a few hundred rows, against real storage and the interpreter:
      - aggregates over few, large groups with order-dependent float sums and integer overflow;
      - index lookup joins of every kind on primary keys, primary key prefixes and unique and non-unique indexes, with NULL keys, residuals, extra keys and both outcomes of the lookup decision;
      - quantified comparisons with NULLs, correlated or not, decorrelated or not.
    - A storage test compares 600 random batches of inserts, updates and deletes (colliding keys within and across batches, NOT NULL failures) with single-row writes: results, errors, rows and indexes.
    - Unit tests compare float and integer folds with the interpreter on adversarial values.
    - Targeted tests check that blocking operators let other tasks run on a single-threaded runtime, that lookups make a few calls per batch and read at most twice the left rows of the right input while deciding, that large inputs become hash joins, and that grouped aggregates run in a 16 KiB budget where a `DISTINCT` one fails.
    - Optimizer golden files and the IR's optimizer differential test cover the quantified rule.
    - 20 planted bugs, one or more in each new path (float, integer, extreme and boolean folds; NULL keys, right-side filters, pending rows, resumed prefix scans, join kinds, residuals and extra keys in lookups; the batched writes' overlay and index maintenance; the quantified rule's counts and extremes; yielding; DML error order), are each caught.

## Consequences

### Positive
- Aggregations hold memory per group: a 10-million-row aggregation into 1,000 groups reserves 0.6 MB.
- Joins of a few rows with a large keyed table cost what the few rows cost.
- `NOT IN` over a large subquery is set-oriented.
- One large query no longer starves other tasks on its thread.

### Negative
- The lookup decision reads up to twice the left rows of the right input speculatively.
- Lookups run several storage calls per batch concurrently, so one query can occupy several storage threads.
- The quantified rule reads its subquery twice for `=`.

## Open Questions

- None.

## Future Work

- Decode on the storage thread (ADR-0023 phase 3, backlog `batch-decoder`).
- Statistics would let the planner choose lookups up front instead of racing the inputs.

## References

- `mvp-completion.md`, F3; `executor-design.md`
- [ADR-0022](0022-vectorized-arrow-stream-executor.md), [ADR-0023](0023-leaf-operators-and-storage-pushdown.md), [ADR-0026](0026-statement-atomicity-and-halloween-protection.md), [ADR-0027](0027-query-memory-management.md), [ADR-0028](0028-physical-operators-planning-and-explain.md), [ADR-0012](0012-nesting-and-decorrelation-for-graphql.md), [ADR-0014](0014-rule-based-optimizer.md)
