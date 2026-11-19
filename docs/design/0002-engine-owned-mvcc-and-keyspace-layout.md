---
id: 0002
title: Engine-Owned MVCC and Keyspace Layout
status: implemented  # proposal | accepted | implemented | superseded
tags: [storage, mvcc, fjall]
feature-flag:
created: 2026-11-17
accepted: 2026-11-18
implemented: 2026-11-18
spec-sections:
  - mvp-fjall-storage-design.md#keyspace-layout-and-key-encoding
  - mvp-fjall-storage-design.md#fjall-keyspaces
  - mvp-fjall-storage-design.md#versioned-key-layout
superseded-by:
---

# ADR-0002: Engine-Owned MVCC and Keyspace Layout

## Status

Implemented

## Summary

We own MVCC. The engine uses a plain fjall `Database` and writes versioned keys itself (`tuple ++ !commit_ts`) through fjall's atomic cross-keyspace write batches, rather than relying on fjall's `OptimisticTxDatabase` or `SingleWriterTxDatabase`. Data lives in a versioned `data` keyspace (including the system catalog) and engine state in an unversioned `meta` keyspace.

## Context

fjall offers transactional database variants (`OptimisticTxDatabase`, `SingleWriterTxDatabase`), but their sequence numbers are local to one process. That's a problem for the system's later goals:

- Raft will apply the same log on several replicas; versions must mean the same thing on each.
- Reads must be possible at a chosen timestamp.
- SSI with range tracking ([ADR-0004](0004-optimistic-transactions-and-isolation-levels.md)) needs control over versioning and conflict detection that fjall's transactions don't provide.

## Decision

Use a plain fjall `Database` and implement MVCC with versions stored in the keys.

### fjall keyspaces

| Keyspace | Holds | Versioned |
| --- | --- | --- |
| `data` | All rows and index entries, including system catalog tables | Yes |
| `meta` | Engine state: format version, last commit timestamp, GC watermark | No |
| `raftlog` (later) | Raft log entries | No |

The catalog lives in `data` under reserved table IDs, so DDL is transactional for free ([ADR-0007](0007-minimal-catalog-and-row-layer.md)).

### Versioned key layout

```
data key = tuple(table_id, index_id, key columns...) ++ !commit_ts
```

- `commit_ts` is a big-endian `u64` with all bits inverted, so a key's versions sort newest first and sit next to each other.
- The tuple part is self-delimiting ([ADR-0003](0003-order-preserving-tuple-encoding-and-row-format.md)), so no separator is needed before the version.
- The value is a tagged record: `Put(row bytes)` or `Tombstone`.

### `commit_ts` is opaque

- `commit_ts` is an opaque, totally ordered `u64`. Code outside the sequencer (the timestamp source) only compares timestamps; it never does arithmetic on them.
- The MVP source is a logical counter ([ADR-0004](0004-optimistic-transactions-and-isolation-levels.md)). A later Raft-derived timestamp or hybrid logical clock fits the same 64 bits: an HLC would pack 48 bits of physical milliseconds and a 16-bit logical counter. Changing the timestamp source therefore needs no key-format change.

### Reads

- A read at snapshot `ts` seeks to `tuple ++ !ts` and takes the first version at or below `ts`.
- A scan skips older versions of each key as it goes.

## Implementation Phases

- [x] **Phase 1: Keyspaces and `meta`** - M1
- [x] **Phase 2: MVCC reads and versioned scans** - M2

## Consequences

### Positive
- Versions remain meaningful across replicas when Raft applies the same log.
- Reads at an arbitrary snapshot timestamp are a single seek.
- Full control over isolation and range-conflict tracking.
- Transactional DDL with no extra machinery.

### Negative
- Write amplification: our MVCC versions sit on top of fjall's own LSM versioning. Acceptable for small workloads; GC ([ADR-0008](0008-version-garbage-collection.md)) keeps it bounded.
- We must implement and test visibility, version GC and conflict detection ourselves.

## Open Questions

- None.

## Future Work

- `raftlog` keyspace for Raft log entries.
- `commit_ts` derived from the Raft log index.

## References

- `mvp-fjall-storage-design.md` — Keyspace layout and key encoding
- [ADR-0001: fjall Storage Engine and Async/Sync Boundary](0001-fjall-storage-engine-and-async-sync-boundary.md)
- [ADR-0003: Order-Preserving Tuple Encoding and Row Format](0003-order-preserving-tuple-encoding-and-row-format.md)
- [ADR-0004: Optimistic Transactions and Isolation Levels](0004-optimistic-transactions-and-isolation-levels.md)
- [ADR-0007: Minimal Catalog and Row Layer](0007-minimal-catalog-and-row-layer.md)
- [ADR-0008: Version Garbage Collection](0008-version-garbage-collection.md)
