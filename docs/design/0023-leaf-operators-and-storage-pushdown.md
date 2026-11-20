---
id: 0023
title: Leaf Operators and Storage Pushdown
status: accepted  # proposal | accepted | implemented | superseded
tags: [executor, storage, pushdown]
feature-flag:
created: 2026-11-17
accepted: 2026-11-19
implemented:
spec-sections: ["executor-design.md#where-rows-become-columns", "executor-design.md#physical-operators", "executor-design.md#physical-planning-mvp"]
superseded-by:
---

# ADR-0023: Leaf Operators and Storage Pushdown

## Status

Accepted

## Summary

Only leaf operators (`TableScan`, `IndexScan`, `IndexLookup`) call the storage layer, always one call per batch through the transaction's `Txn` handle. Filters, projections and limits are pushed into `ScanRequest` where allowed. In the MVP, encoded rows are decoded into Arrow on the async side; a later `BatchDecoder` extension to `ScanRequest` moves decode, filter and projection onto the storage thread.

## Context

The storage trait ([ADR-0006](0006-batched-async-storage-trait.md)) is coarse-grained: `scan(ScanRequest)` with range, filter, projection, limits and resume key, and `get_many` for point reads. It returns encoded rows ([ADR-0003](0003-order-preserving-tuple-encoding-and-row-format.md)), while the executor works on Arrow batches ([ADR-0022](0022-vectorized-arrow-stream-executor.md)). Two questions follow: which operators may touch storage, and where rows become columns. Every row copied across the sync/async boundary only to be filtered out is wasted work.

## Decision

**Leaves are the only storage callers.** Everything above them is CPU work on batches.

| Leaf | Storage call | Notes |
| --- | --- | --- |
| `TableScan` | `scan` | Primary key range; pushed-down filter, projection and limit go into `ScanRequest`; follows `resume` keys until `None` |
| `IndexScan` | `scan` | Secondary index range, yielding primary keys and index columns |
| `IndexLookup` | `get_many` | Fetches rows by primary key, one call per input batch |

- One storage call per batch, never per row.
- All leaves read through the `Txn` handle, so results include the transaction's own buffered writes ([ADR-0026](0026-statement-atomicity-and-halloween-protection.md)).
- Pushdown into `ScanRequest` is subject to the barrier rules: only leakproof predicates and the policy residual may run in the storage `RowFilter` ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md)). Only the single-row part of a policy residual is pushed; conjuncts that need a join (foreign-key paths, `in` semi-joins on the closure table) stay in a `PolicyFilter` above that join.
- **Row-level filter evaluation** *(recommended in review)*. Until `BatchDecoder` exists, pushed-down `RowFilter`s evaluate row at a time using the reference interpreter's expression evaluator ([ADR-0021](0021-ir-correctness-and-security-testing.md)). Differential tests check it against the vectorized kernels ([ADR-0024](0024-expression-evaluation-with-postgres-semantics.md)), so there is one row-level implementation of the semantics rather than a second hand-written one.

**Where rows become columns.**

1. **MVP:** the scan operator decodes rows into Arrow on the async side, then applies any filter that wasn't pushed into storage.
2. **Later:** a `BatchDecoder` extension to `ScanRequest` decodes, filters and projects on the storage thread and returns Arrow directly, so rows that fail the filter are never copied across the boundary. This is a planned change to the storage trait ([ADR-0006](0006-batched-async-storage-trait.md)). Storage defines the `BatchDecoder` trait; the Arrow-based implementation lives in the executor crate, so the storage crate never depends on Arrow.

## Implementation Phases

- [x] **Phase 1: TableScan with ScanRequest pushdown and async-side decode** - E1
  - *As implemented:* the planner splits a filter directly above a scan into leakproof conjuncts, run on the storage thread through a `RowFilter` (new `rows::scan_filtered`), and the rest, which run in a `Filter` above the scan. Because pushed conjuncts can discard a row before a raising conjunct sees it, a query the interpreter fails may succeed in the executor; this matches Postgres, which does not fix conjunct order. Stored tables are found by name until the storage-backed catalog (ADR-0013 phase 3) exists.
- [x] **Phase 2: IndexScan and IndexLookup** - E4
  - *As implemented:* `TableScan` carries an access path chosen from the conjuncts of a filter directly on it (`interlaken_exec::Access`): every row; a primary key prefix and range (one `scan` per batch); or a secondary index prefix and range (one `scan_index` for a batch of primary keys, then one `get_many` for their rows). The row layer gained `RowScan::range`, a value range on the key column after the prefix, which becomes key bounds on each value's block of keys (swapped for descending columns) with the NULL block cut off. Key values come from expressions that read none of the scanned table's columns and cannot raise (literals, parameters, outer columns), so a correlated `Apply` whose inner filter is keyed by the outer row is an index nested loop. Float columns are never keys, because -0 and NaN make key equality differ from SQL equality. The filter always stays as a recheck. Index scans cannot use a storage `RowFilter`, so under them every conjunct runs above the scan. A batched `IndexLookup` that probes with a whole input batch, for joins, is future work. *F3 built it ([ADR-0033](0033-executor-performance.md)):* an `IndexLookup` join fetches the right rows for a whole left batch with `get_many` (a whole primary key) or one `scan_many` of prefix scans (a key prefix or secondary index, then `get_many`), in concurrent calls of at most 256 keys.
- [ ] **Phase 3: BatchDecoder on the storage thread** - F3 ([mvp-completion.md](mvp-completion.md)), only if decode shows up in its profiles
  - *As measured in F3:* not needed yet. A profile of the million-row scan benchmarks (`count(*)`, and a filter the executor evaluates) put 89% of CPU samples on storage pool threads (fjall iteration and MVCC version resolution) and 11% on executor threads, where row decoding (`decode_row`, `from_stored`, `build_array` and their allocations) was about 6% of the total. A scan is bound by its storage calls, so moving decode onto the storage thread would lengthen them, and pushed filters already keep rejected rows in the storage pool. Deferred to backlog `batch-decoder`, which starts with decoding straight from the row format into Arrow builders.

## Consequences

### Positive
- A single, auditable boundary between the executor and storage.
- Batched calls and pushdown keep cross-boundary traffic proportional to batches, not rows.
- Pushing the single-row part of the policy residual into the scan means rows it rejects never leave the storage pool ([ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md)).

### Negative
- Phase 1 decodes on executor threads and copies rows that may then be filtered out. If this shows up in profiles, `BatchDecoder` must be brought forward.
- `BatchDecoder` changes the storage trait, coupling it to Arrow.

## Open Questions

- None.

## Future Work

- `BatchDecoder` in `ScanRequest`.
- Merge join exploiting index order.

## References

- `executor-design.md` — Where rows become columns, Physical operators, Physical planning
- [ADR-0003](0003-order-preserving-tuple-encoding-and-row-format.md), [ADR-0006](0006-batched-async-storage-trait.md), [ADR-0007](0007-minimal-catalog-and-row-layer.md), [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md), [ADR-0022](0022-vectorized-arrow-stream-executor.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md), [ADR-0028](0028-physical-operators-planning-and-explain.md)
