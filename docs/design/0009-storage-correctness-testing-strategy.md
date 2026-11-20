---
id: 0009
title: Storage Correctness Testing Strategy
status: implemented  # proposal | accepted | implemented | superseded
tags: [storage, testing]
feature-flag:
created: 2026-11-17
accepted: 2026-11-18
implemented: 2026-11-19
spec-sections:
  - mvp-fjall-storage-design.md#testing-strategy
  - mvp-fjall-storage-design.md#milestones
superseded-by:
---

# ADR-0009: Storage Correctness Testing Strategy

## Status

Implemented

## Summary

Correctness is the product, so the storage test harness gets as much attention as the engine. Each layer has a dedicated technique: property tests for encoding, fuzzing for decoders, model-based tests against an in-memory reference for MVCC and scans, Jepsen Elle history checking for isolation (including a negative test), a crash harness for durability, and a deterministic in-memory `Storage` for simulating upper layers.

## Context

The MVP's success criteria are that concurrent transactions show no anomalies under SERIALIZABLE (checked by an independent history checker across thousands of randomized runs), that committed data survives process kill at any point in the commit path, and that scans return correct, ordered results at snapshot isolation. These properties can't be established by example-based unit tests alone.

## Decision

| Layer | Technique | Catches |
| --- | --- | --- |
| Tuple encoding | Property tests (`proptest`): encoded byte order equals value order for every type, DESC and NULL placement, composite tuples; round-trip decode | Ordering and escaping bugs |
| Decoder | Fuzzing (`cargo fuzz`) on keys and row values | Panics on corrupt input |
| MVCC and scans | Model-based testing against an in-memory reference store (`BTreeMap` of versions with serial semantics); random operation sequences, results compared | Visibility, resume and merge bugs |
| Isolation | Concurrent randomized workloads (list-append, read-write registers); histories exported to Jepsen's Elle checker | G0, G1a–c, G-single, G2-item, phantoms |
| Durability | Crash harness: kill the process at random points in the commit path, restart, check every acknowledged commit is present and no group is partially applied | Lost or torn commits |
| Upper layers (later) | A deterministic in-memory `Storage` implementation behind the same trait, for `madsim`/`turmoil` simulation | Bugs above storage, replayable from a seed |

- **Negative isolation tests:** under REPEATABLE READ, the checker should *find* write skew (G2-item), proving the harness can detect anomalies.
- **As implemented (M6):**
  - `tests/elle.rs` records list-append histories and checks them with `elle-cli` (found via `ELLE_CLI` or `target/elle-cli.jar`). SERIALIZABLE must be strict-serializable, REPEATABLE READ must satisfy snapshot isolation, and READ COMMITTED must satisfy read-committed. REPEATABLE READ is also checked as serializable, where Elle must find G2-item: that is the negative test. Without Java or the jar, histories are saved and the check is skipped.
  - `tests/crash.rs` re-runs its own binary as a child committing under `Sync` with GC running, kills it with `SIGKILL` at random times, and verifies after recovery that no acknowledged commit is lost, no transaction is torn and the commit clock resumes correctly; it repeats on the same directory. `SIGKILL` does not simulate power loss, so fsync behaviour is not covered (F4 added that, below).
  - The exit run: 1,200 Elle histories, 300 crash cycles, 200 runs each of the serializability, uniqueness, GC and catalog suites, and 13,000 model-test cases, with no anomalies, lost commits or hangs. `ELLE_RUNS`, `CRASH_CYCLES` and `PROPTEST_CASES` scale each part.
  - An independent code review during hardening found two bugs, both fixed with regression tests: READ COMMITTED dependency validation against a refreshed snapshot ([ADR-0004](0004-optimistic-transactions-and-isolation-levels.md)), and a stranded low-priority pool job after a cancelled submission ([ADR-0001](0001-fjall-storage-engine-and-async-sync-boundary.md)).
- **As implemented (F4): power loss and disk space.**
  - `tests/crash.rs` now runs its workload four ways. Process crashes under `Sync` and under `Buffered` must lose nothing. Power loss under `Sync` and under `Buffered` runs on a [LazyFS](https://github.com/dsrhaslab/lazyfs) mount (`tests/support/lazyfs.rs`). After each `SIGKILL` the parent sends `lazyfs::clear-cache`, which drops every write not yet synced, as a power cut does. LazyFS keeps directory operations, so this models lost file contents, not lost directory entries.
  - The checks are stricter than in M6. A transaction counts as committed if its keys are present or a committed delete removed them. Every survivor must be at or below the recovered commit clock and every lost commit above it, so survivors form a prefix of the commit order. Under `Sync`, or after a process crash, nothing acknowledged may be lost.
  - Power-loss children use 1 MiB memtables, so fjall flushes tables and rotates journals often and power loss also strikes those paths. That also makes `Buffered` keep part of its history, so the prefix check has something to bite on: with fjall's default memtables nothing is ever synced and every `Buffered` commit is lost.
  - The power-loss tests need `LAZYFS_ROOT`, a LazyFS checkout built by `scripts/build-lazyfs.sh` (no root needed: if the FUSE 3 headers are missing it extracts `libfuse3-dev` from `apt-get download`). Without it they print a skip message and pass. `scripts/power-loss.sh` runs the whole file with `CRASH_CYCLES=50`.
  - Results: 20 cycles each. `Sync` lost none of 119,467 acknowledged commits. `Buffered` lost 16,200 of 183,822, always a suffix and never torn. Planting `Buffer` in place of `SyncAll` for `Sync` failed at once.
  - The harness found one bug: `Buffered` commits were lost on a plain process crash, because no persist mode was passed to fjall. It is fixed, with `crash_recovery_loses_no_buffered_commits` as the regression test ([ADR-0005](0005-commit-sequencer-group-commit-and-durability.md)).
  - `tests/disk_space.rs` asserts `disk_space_bytes` levels off under sustained updates: 12 seconds in the normal suite, `SOAK_SECS` (default 600) in `scripts/disk-space-soak.sh`. Results are in [ADR-0008](0008-version-garbage-collection.md).
  - Both scripts exit non-zero on failure, so CI can run them nightly. The repository has no CI configuration yet.
- **Benchmarks:** baseline benchmarks (point reads, scans, commit throughput by group size) run in CI to catch regressions. They are not optimization targets for the MVP.

## Implementation Phases

- [x] **Phase 1: Encoding property tests and fuzzing** - M1
- [x] **Phase 2: Model-based tests for single transactions** - M2
- [x] **Phase 3: Model-based tests with concurrency** - M3
- [x] **Phase 4: Concurrent uniqueness tests** - M4
- [x] **Phase 5: Bounded-space test under sustained updates** - M5
- [x] **Phase 6: Elle checking, crash harness, hardening** - M6
- [x] **Phase 7: Power-loss testing on LazyFS, disk-space soak** - F4

## Consequences

### Positive
- Independent checkers validate isolation claims rather than the implementation testing itself.
- The negative test guards against a harness that silently passes.
- The in-memory `Storage` lets upper layers be tested deterministically from a seed.

### Negative
- Significant engineering effort (M6 alone is 2–3 weeks).
- Elle integration requires exporting histories in a JVM-tooling-compatible format.

## Open Questions

- None specific to this ADR.

## Future Work

- Deterministic simulation of the storage engine itself once fjall is forked with injected traits (option 2).
- Performance tuning beyond regression baselines.

## References

- `mvp-fjall-storage-design.md` — Testing strategy; Milestones
- [ADR-0003: Order-Preserving Tuple Encoding and Row Format](0003-order-preserving-tuple-encoding-and-row-format.md)
- [ADR-0004: Optimistic Transactions and Isolation Levels](0004-optimistic-transactions-and-isolation-levels.md)
- [ADR-0005: Commit Sequencer, Group Commit and Durability](0005-commit-sequencer-group-commit-and-durability.md)
- [ADR-0006: Batched Async Storage Trait](0006-batched-async-storage-trait.md)
- Jepsen Elle: https://github.com/jepsen-io/elle
- `proptest`, `cargo fuzz`, `madsim`, `turmoil`
