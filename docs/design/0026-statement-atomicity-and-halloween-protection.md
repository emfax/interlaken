---
id: 0026
title: Statement Atomicity and Halloween Protection
status: proposal  # proposal | accepted | implemented | superseded
tags: [executor, transactions, dml]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["executor-design.md#transactions-and-statements", "executor-design.md#testing-strategy"]
superseded-by:
---

# ADR-0026: Statement Atomicity and Halloween Protection

## Status

Proposal

## Summary

Every query runs inside a transaction and reads through its `Txn` handle, including its own uncommitted writes. Before each statement the executor calls `next_statement()` and takes an implicit savepoint; a failed statement rolls back to it and leaves the transaction in a Postgres-style aborted state. DML writes are tagged with a statement sequence number and scans ignore writes from the current statement, preventing the Halloween problem. The executor never retries serialization failures itself.

## Context

The storage layer provides a write buffer merged into reads, savepoints, `next_statement()` for READ COMMITTED snapshot refresh, and retryable `SerializationFailure` (`40001`) at commit ([ADR-0004](0004-optimistic-transactions-and-isolation-levels.md), [ADR-0006](0006-batched-async-storage-trait.md)). The executor must map SQL statement semantics onto these. Because scans see the transaction's own writes, an `UPDATE` that moves rows forward in the key order it is scanning could see and update the same row again (the Halloween problem) and might never terminate.

## Decision

- **Reads.** Every leaf reads through the transaction's `Txn` handle, so results include the transaction's own uncommitted writes.
- **Statement boundaries.** Before each statement the executor calls `next_statement()` (refreshing the snapshot under READ COMMITTED) and takes an implicit savepoint.
- **Statement atomicity.** A failing statement rolls back to its savepoint. As in Postgres, the transaction then enters an aborted state until the client rolls back, fully or to an explicit savepoint.
- **Halloween protection.** DML reads the write buffer as of the start of the statement. The statement sequence tag lives in the storage write buffer ([ADR-0004](0004-optimistic-transactions-and-isolation-levels.md), [ADR-0006](0006-batched-async-storage-trait.md)): each entry records the statement that wrote it, `next_statement()` increments the counter, and reads merge only entries from earlier statements. Savepoint undo works unchanged.
- **DML sinks.** `Insert`, `Update` and `Delete` buffer writes into the transaction and maintain indexes through the row layer ([ADR-0007](0007-minimal-catalog-and-row-layer.md)), after `WriteCheck` ([ADR-0025](0025-runtime-security-enforcement-in-executor.md)).
- **Retries.** A `SerializationFailure` at commit is returned to the caller with SQLSTATE `40001`. Retry policy belongs to the client or the WASM host.

**Testing.** Halloween tests: updates that move rows within the scanned key range terminate and touch each row once. Statement tests cover rollback to the implicit savepoint and the aborted state.

## Implementation Phases

- [ ] **Phase 1: DML sinks, statement atomicity, Halloween protection** - E4

## Consequences

### Positive
- Postgres-compatible statement and transaction-abort semantics.
- Halloween protection without materializing DML input.
- No hidden retries: callers keep control of idempotency and retry policy.

### Negative
- The write buffer must carry a statement sequence number per write, an addition to the storage-side `Txn` write buffer ([ADR-0004](0004-optimistic-transactions-and-isolation-levels.md)).
- An implicit savepoint per statement adds undo-log bookkeeping.
- Clients must handle `40001` retries themselves.

## Open Questions

- None.

## Future Work

- Upsert and `MERGE` once added to the IR.

## References

- `executor-design.md` — Transactions and statements, Testing strategy
- [ADR-0004](0004-optimistic-transactions-and-isolation-levels.md), [ADR-0006](0006-batched-async-storage-trait.md), [ADR-0007](0007-minimal-catalog-and-row-layer.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md)
- The Halloween problem (System R, 1976)
