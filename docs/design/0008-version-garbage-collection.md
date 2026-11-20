---
id: 0008
title: Version Garbage Collection
status: implemented  # proposal | accepted | implemented | superseded
tags: [storage, mvcc, gc]
feature-flag:
created: 2026-11-17
accepted: 2026-11-18
implemented: 2026-11-18
spec-sections:
  - mvp-fjall-storage-design.md#version-garbage-collection
superseded-by:
---

# ADR-0008: Version Garbage Collection

## Status

Implemented

## Summary

Obsolete MVCC versions are removed by a background GC task that scans `data` in chunks on the storage pool at low priority and issues physical deletes, guided by a watermark equal to the oldest active snapshot. fjall compaction filters are not used in the MVP.

## Context

Engine-owned MVCC ([ADR-0002](0002-engine-owned-mvcc-and-keyspace-layout.md)) keeps every version until something removes it. Without GC, space grows unboundedly under updates. Resumable scans ([ADR-0006](0006-batched-async-storage-trait.md)) rely on versions at or above the oldest active snapshot staying present.

Alternative considered: **fjall custom compaction filters**. A filter sees one SST run at a time, while the newer version of a key may sit in another level, making obsolescence decisions error-prone.

## Decision

- **Watermark:** the minimum snapshot timestamp of all active transactions, or `visible_ts` when none are active. Persisted in `meta` periodically.
- **Removable versions:**
  - A version is obsolete when a newer version of the same key exists with `commit_ts` at or below the watermark.
  - A tombstone at or below the watermark with no older versions left is also removable.
- **Mechanism:** the GC task scans `data` in chunks on the storage pool at low priority and issues physical deletes in fjall write batches. Physical deletes aren't versioned; they only remove versions no snapshot can see.
- **Scheduling:** GC shares the single storage pool with reads ([ADR-0001](0001-fjall-storage-engine-and-async-sync-boundary.md)). Its chunks are queued at low priority, with a cap on GC tasks in flight. A dedicated GC pool is added only if metrics show GC hurting read latency.
- **Pressure valve:** the maximum transaction age (default 5 minutes, [ADR-0004](0004-optimistic-transactions-and-isolation-levels.md)) caps how far one transaction can hold back the watermark.

### As implemented (M5)

- **Every transaction pins its snapshot,** read-only ones included, in a registry on the commit clock. The watermark is the oldest pin, or `visible_ts` with none. A snapshot is taken under the registry lock, so a new snapshot is never older than a watermark already computed.
- **Expiry revokes pins.** Computing the watermark revokes pins past the maximum age, so they stop holding GC back. A revoked transaction fails its next operation with `TransactionTooOld`. Reads check again after the storage call returns, so a read that raced GC throws away its result rather than returning partial data: revocation happens before any deletion that relies on it. The same watermark trims the sequencer's validation history.
- **Low priority without a second pool.** GC chunks go through the pool's single queue, tagged low priority. The worker that takes one first runs the normal jobs already waiting (up to the queue length at that moment, so GC can't be starved), then the GC chunk. The GC driver keeps one chunk in flight. Chunks end on key boundaries, after `gc_chunk_versions` versions (default 4,096).
- **Schedule.** A background thread runs a pass every `gc_interval` (default 10 s, `None` disables it). `FjallStorage::run_gc` runs a pass on demand and returns a `GcReport`. Each completed pass writes its watermark to `meta`.
- **Metrics.** `FjallStorage::metrics()` returns `StorageMetrics`: visible timestamp, active transactions, oldest snapshot, expired-transaction count, commit pipeline counters, GC counters (passes, failures, versions examined and removed, last pass) and disk usage.

### As implemented (F4): disk space levels off

- **Soak.** `tests/disk_space.rs` overwrites a fixed key set at a target rate with GC every 100 ms and samples `disk_space_bytes`. It asserts that, after a warm-up third, the later half of the run peaks at most 25% (plus 8 MiB) above the earlier half. A 12-second version runs in the normal suite; the long one is `#[ignore]`d and run by `scripts/disk-space-soak.sh` (`SOAK_SECS`, default 600).
- **Result.** Space levels off, but the footprint is dominated by fjall's journals, not by MVCC garbage. It saws between roughly the live data and fjall's journal cap (512 MiB by default) plus memtables: journals are deleted only once the memtables they cover are flushed. A 300 s run with default limits at 16 MiB/s wrote 4.8 GiB and GC removed 9.6 million versions. Disk use ranged 80–696 MiB once warm, with per-minute peaks of 577, 684, 696, 568 and 645 MiB: no growth trend. With a 64 MiB journal cap and 4 MiB memtables the short test levels off at 70–160 MiB.
- **Limits are configurable.** Because the journal cap sets the steady-state footprint, `StorageOptions` gained `max_journal_bytes` (default 512 MiB, at least 64 MiB) and `memtable_bytes` (default 64 MiB, at least 1 MiB; fjall records it when the database is created). An embedded deployment that cares about disk footprint should lower both.

## Implementation Phases

- [x] **Phase 1: Watermark tracking and background GC scanner** - M5

## Consequences

### Positive
- Correctness rules are simple and testable in isolation.
- Space stays bounded under sustained update workloads.
- Resumable scans remain safe without held fjall snapshots.

### Negative
- GC scans consume storage-pool capacity and add write traffic.
- Long-running transactions delay reclamation until the age limit.

## Open Questions

- None.

## Future Work

- Compaction-filter-based GC as an optimization once the GC rules are proven by tests.

## References

- `mvp-fjall-storage-design.md` — Version garbage collection
- [ADR-0001: fjall Storage Engine and Async/Sync Boundary](0001-fjall-storage-engine-and-async-sync-boundary.md)
- [ADR-0002: Engine-Owned MVCC and Keyspace Layout](0002-engine-owned-mvcc-and-keyspace-layout.md)
- [ADR-0004: Optimistic Transactions and Isolation Levels](0004-optimistic-transactions-and-isolation-levels.md)
- [ADR-0006: Batched Async Storage Trait](0006-batched-async-storage-trait.md)
