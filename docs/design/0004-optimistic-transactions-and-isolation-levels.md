---
id: 0004
title: Optimistic Transactions and Isolation Levels
status: proposal  # proposal | accepted | implemented | superseded
tags: [storage, transactions, isolation, mvcc]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections:
  - mvp-fjall-storage-design.md#transaction-layer
  - mvp-fjall-storage-design.md#timestamps
  - mvp-fjall-storage-design.md#transaction-state
  - mvp-fjall-storage-design.md#commit-validation
  - mvp-fjall-storage-design.md#savepoints
  - mvp-fjall-storage-design.md#limits
superseded-by:
---

# ADR-0004: Optimistic Transactions and Isolation Levels

## Status

Proposal

## Summary

Transactions run optimistically against MVCC snapshots, buffer writes on the async side, record a read set of point keys and actually-covered ranges, and are validated at commit by a single sequencer using backward validation. READ COMMITTED, REPEATABLE READ (snapshot isolation) and SERIALIZABLE are supported; under SERIALIZABLE the serial order equals the commit order. Read-only transactions skip validation.

## Context

The engine must offer serializable isolation with range-conflict tracking (no phantoms), plus the weaker Postgres levels, on top of engine-owned MVCC ([ADR-0002](0002-engine-owned-mvcc-and-keyspace-layout.md)). Later layers depend on this: the security core registers policy reads in the read set so revocations conflict with dependent transactions, and Raft will replace the timestamp source.

Alternatives considered:

- **Full SSI** (Cahill et al.), tracking dangerous structures of incoming and outgoing read-write dependencies — fewer aborts, more complexity.
- **Pessimistic row locks / serial execution** — deferred.

Backward validation is chosen for simplicity; it is stricter than SSI but still serializable.

## Decision

### Timestamps

- `commit_ts`: a logical `u64` assigned by the sequencer, increasing by one per committed transaction. Outside the sequencer it is treated as opaque and only compared ([ADR-0002](0002-engine-owned-mvcc-and-keyspace-layout.md)).
- `visible_ts`: the highest `commit_ts` whose writes are already in fjall. New snapshots read at `visible_ts`, so readers never see a timestamp whose data isn't readable yet.
- Later, Raft replaces the sequencer: the log index (or a timestamp derived from it) becomes `commit_ts`, and the rest of the layer stays the same.

### Transaction state

- Snapshot timestamp and isolation level.
- **Write buffer:** an ordered map of key to `Put` or `Delete`. Each entry also records the statement number that wrote it. Reads merge the buffer over the snapshot, so a transaction reads its own writes from earlier statements.
- **Statement counter:** incremented by `next_statement()`.
- **Read set:** point keys plus key ranges. A scan records the range it actually covered — up to its resume key if it stopped early — not the range it was asked for.
- **Savepoint stack:** positions in the write buffer's undo log.

### Commit validation

The sequencer keeps the write sets of recently committed transactions, ordered by `commit_ts`, trimmed below the oldest active snapshot. A transaction with snapshot `s` aborts if any transaction committed after `s` wrote:

| Isolation level | Conflicts checked |
| --- | --- |
| SERIALIZABLE | A key in its read set or inside a read range, or a key it also writes |
| REPEATABLE READ (snapshot isolation) | A key it also writes (first committer wins) |
| READ COMMITTED | A key it also writes; the snapshot refreshes at each statement boundary (`txn.next_statement()`) |
| All levels | A key or range registered as a **dependency** |

- Backward validation aborts on any read-write conflict with a later committer, not only on dangerous structures.
- **Read-only transactions skip validation entirely:** a read-only snapshot is a consistent point in the commit order.
- Range checks use an ordered set of committed write keys queried with a range lookup.
- **Dependencies** (added in M4) are reads validated at every isolation level, registered with `register_dependency` / `register_dependency_range`. A conflict returns `Conflict::DependencyChanged`. The row layer needs them to keep indexes correct under weak isolation: DML depends on its table's catalog row, and DDL that reads table data (index backfill, drops) depends on the ranges it read ([ADR-0007](0007-minimal-catalog-and-row-layer.md)). Without them, a REPEATABLE READ insert using a stale table definition could commit a row that a concurrent `CREATE INDEX` backfill never saw.
- READ COMMITTED validates each written key against the snapshot of the statement that first wrote it, so a write made after a snapshot refresh does not conflict with commits that statement already saw.

**Known deviation from Postgres:** under READ COMMITTED, Postgres waits on a conflicting writer and re-checks the row (EvalPlanQual). The MVP aborts with SQLSTATE `40001` instead.

### Statement boundaries

- `next_statement()` increments the statement counter. Under READ COMMITTED it also takes a fresh snapshot at `visible_ts`. This is the only point where READ COMMITTED refreshes, as in Postgres. Refreshing per scan call was rejected because the two sides of one join could then see different snapshots.
- **Reads ignore the current statement's writes.** Reads and scans merge only write-buffer entries from earlier statements, like Postgres command IDs. A statement never sees its own writes, so an `UPDATE` that moves rows forward in the key order it is scanning can't reach them again (Halloween protection, [ADR-0026](0026-statement-atomicity-and-halloween-protection.md)).
- Writes from the current statement still count for commit validation and become visible to the next statement.

### Savepoints

`ROLLBACK TO` truncates the write buffer back to the savepoint's undo-log position. Reads made after the savepoint stay in the read set — conservative, because the transaction may already have acted on what it read. Statement tags don't change undo: truncation removes entries regardless of their statement number.

### Limits

- Maximum write-set size per transaction: default 64 MB.
- Maximum transaction age: default 5 minutes, so a forgotten transaction can't hold back version GC indefinitely.
- As implemented (M5): both limits are `StorageOptions` fields. The write-set size counts the final buffered write per key (key plus value bytes); a write that would exceed it fails with `WriteSetTooLarge` (SQLSTATE `54000`). The age limit covers read-only transactions too, since they also hold back GC. An over-age transaction fails its next read, write or commit with `TransactionTooOld` (SQLSTATE `25P04`). See [ADR-0008](0008-version-garbage-collection.md).
- Aborts return a typed `SerializationFailure` (`40001`) marked as retryable.

## Implementation Phases

- [x] **Phase 1: Write buffer, read-your-writes, MVCC snapshot reads** - M2
- [x] **Phase 2: Validation for all three isolation levels, savepoints** - M3
- [x] **Phase 3: Transaction limits** - M5

## Consequences

### Positive
- Serializability with phantom protection via range tracking.
- Simple validation logic; serial order equals commit order.
- Read-only transactions never abort.
- Read-set registration gives later layers (security policy reads) conflict detection for free.
- Timestamp source can be swapped for Raft without changing the transaction layer.

### Negative
- Abort rate is higher than Postgres SSI. If it hurts, upgrade to full SSI without changing the API.
- READ COMMITTED aborts instead of re-checking, which may surprise Postgres users.
- Write buffers and read sets live in memory; large `get_many` read sets may need coalescing into ranges.
- Ordered-set range checks may need an interval index at larger scale.

## Open Questions

- None.

## Future Work

- Pessimistic row locks (and revisiting the READ COMMITTED deviation with them).
- Serial execution mode.
- Full SSI with incoming/outgoing dependency tracking.
- Interval index for range conflict checks.
- Raft-derived `commit_ts`.

## References

- `mvp-fjall-storage-design.md` — Transaction layer
- [ADR-0002: Engine-Owned MVCC and Keyspace Layout](0002-engine-owned-mvcc-and-keyspace-layout.md)
- [ADR-0005: Commit Sequencer, Group Commit and Durability](0005-commit-sequencer-group-commit-and-durability.md)
- [ADR-0006: Batched Async Storage Trait](0006-batched-async-storage-trait.md)
- [ADR-0008: Version Garbage Collection](0008-version-garbage-collection.md)
- [ADR-0019: Transactional Authorization and Residual Cache](0019-transactional-authorization-and-residual-cache.md)
- [ADR-0026: Statement Atomicity and Halloween Protection](0026-statement-atomicity-and-halloween-protection.md)
- Cahill, Röhm, Fekete — *Serializable Isolation for Snapshot Databases*
