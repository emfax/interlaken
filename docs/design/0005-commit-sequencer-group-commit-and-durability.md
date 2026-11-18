---
id: 0005
title: Commit Sequencer, Group Commit and Durability
status: proposal  # proposal | accepted | implemented | superseded
tags: [storage, transactions, durability]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections:
  - mvp-fjall-storage-design.md#commit-pipeline-and-durability
  - mvp-fjall-storage-design.md#hooks-for-later-layers
superseded-by:
---

# ADR-0005: Commit Sequencer, Group Commit and Durability

## Status

Proposal

## Summary

A single dedicated commit sequencer thread validates transactions, assigns `commit_ts`, and writes each group of transactions as one fjall write batch (plus the last-commit timestamp in `meta`). Durability is configurable: `Sync` (default) calls `persist(SyncAll)` once per group before acknowledging; `Buffered` acknowledges after writes reach OS buffers.

## Context

Optimistic validation ([ADR-0004](0004-optimistic-transactions-and-isolation-levels.md)) needs a total commit order and a single point that sees all recent write sets. Durable commits require fsync, which is expensive per transaction; group commit amortizes it. fjall's own default only flushes to OS buffers, so durability must be requested explicitly. The design must also allow Raft to later take over the commit path.

## Decision

### Commit pipeline

1. The caller sends its read set, ranges and write set to the sequencer.
2. The sequencer validates the transaction, assigns `commit_ts`, and adds its writes to the current group.
3. The group becomes **one** fjall write batch covering all its transactions, plus the new last-commit timestamp in `meta`.
4. The batch is persisted according to the durability mode. `visible_ts` advances, then every caller in the group gets its acknowledgement.

### Durability modes

| Durability mode | Behaviour | Use |
| --- | --- | --- |
| `Sync` | `persist(SyncAll)` once per group before acknowledging | Default |
| `Buffered` | Acknowledge after the write reaches OS buffers | Development and tests |

`Sync` must call `persist(SyncAll)` explicitly; the crash tests ([ADR-0009](0009-storage-correctness-testing-strategy.md)) verify it.

### Raft hook

The sequencer becomes the leader's proposal path; an apply thread writes the same fjall batches; `commit_ts` derives from the log.

## Implementation Phases

- [x] **Phase 1: Sequencer, group commit, durability modes** - M3
- [x] **Phase 2: Crash harness verification** - M6

## Consequences

### Positive
- Atomic groups: no transaction in a group is partially applied.
- fsync cost amortized across a group.
- `visible_ts` only advances after data is readable, keeping snapshots consistent.
- Clean substitution point for Raft.

### Negative
- One validating thread caps commit rate. Fine at target scale; measure in M3 before optimizing.
- Correctness depends on calling `persist(SyncAll)` and on fjall's fsync discipline for new files.

### Measured (M3)

`crates/storage/benches/commit_throughput.rs`, 22-core machine, NVMe disk, 2 s runs. "blind" is one write per REPEATABLE READ transaction. "rw" is SERIALIZABLE with four point reads, a 16-row scan and one write.

| Workload | Durability | 1 client | 16 clients | 256 clients | Mean group at 256 |
| --- | --- | --- | --- | --- | --- |
| blind | `Sync` | 4.1k/s | 26.6k/s | 225k/s | ~245 |
| blind | `Buffered` | 95k/s | 306k/s | 307k/s | ~242 |
| rw | `Sync` | 3.0k/s | 25.1k/s | 87k/s | ~177 |
| rw | `Buffered` | 16k/s | 106k/s | 93k/s | ~143 |

- **`Sync`:** fsync-bound, not CPU-bound. The sequencer spends almost all its time in the group sync (about 230–970 µs per group); validation costs 1.5–6 µs per transaction. Group commit scales throughput with concurrency. Validating the next group while the current one syncs would help at moderate concurrency.
- **`Buffered`:** here the single thread is the limit: about 300k blind commits/s and 100k SERIALIZABLE read-write commits/s. Validation, history trimming and acknowledgement take 1.5–7 µs per transaction. The cost grows with concurrency because open transactions hold back history trimming.
- Rare commit stalls of 0.2–2 s match raw fsync stalls measured on the same disk (up to 354 ms with plain `fsync`). They come from the disk, not the sequencer; the M6 crash and hardening work should re-check them on other hardware.
- Conclusion: one sequencer is ample for the MVP's target scale. No optimization is needed now.

### Crash harness (M6)

300 random `SIGKILL`s during `Sync` commits with GC running ([ADR-0009](0009-storage-correctness-testing-strategy.md)) lost no acknowledged commit and tore no group, and the commit clock always resumed after the last durable timestamp. Deliberately acknowledging before the group was written, or restarting the clock at zero, failed the harness at once. Power loss (unsynced data in OS buffers) is not simulated; verifying `persist(SyncAll)` needs a fault-injecting filesystem such as LazyFS, left as future work. The fsync stalls noted above were not re-checked on other hardware.

## Open Questions

- None specific to this ADR.

## Future Work

- Raft proposal in place of local commit; apply thread writing the same batches.
- Timestamp oracle and cross-range commits.

## References

- `mvp-fjall-storage-design.md` — Commit pipeline and durability; Hooks for later layers
- [ADR-0001: fjall Storage Engine and Async/Sync Boundary](0001-fjall-storage-engine-and-async-sync-boundary.md)
- [ADR-0004: Optimistic Transactions and Isolation Levels](0004-optimistic-transactions-and-isolation-levels.md)
- [ADR-0009: Storage Correctness Testing Strategy](0009-storage-correctness-testing-strategy.md)
