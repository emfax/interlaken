---
id: 0009
title: Storage Correctness Testing Strategy
status: proposal  # proposal | accepted | implemented | superseded
tags: [storage, testing]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections:
  - mvp-fjall-storage-design.md#testing-strategy
  - mvp-fjall-storage-design.md#milestones
superseded-by:
---

# ADR-0009: Storage Correctness Testing Strategy

## Status

Proposal

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
- **Benchmarks:** baseline benchmarks (point reads, scans, commit throughput by group size) run in CI to catch regressions. They are not optimization targets for the MVP.

## Implementation Phases

- [x] **Phase 1: Encoding property tests and fuzzing** - M1
- [x] **Phase 2: Model-based tests for single transactions** - M2
- [x] **Phase 3: Model-based tests with concurrency** - M3
- [x] **Phase 4: Concurrent uniqueness tests** - M4
- [ ] **Phase 5: Bounded-space test under sustained updates** - M5
- [ ] **Phase 6: Elle checking, crash harness, hardening** - M6

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
