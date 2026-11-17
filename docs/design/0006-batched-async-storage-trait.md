---
id: 0006
title: Batched Async Storage Trait
status: proposal  # proposal | accepted | implemented | superseded
tags: [storage, api]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections:
  - mvp-fjall-storage-design.md#storage-trait-api
  - mvp-fjall-storage-design.md#hooks-for-later-layers
superseded-by:
---

# ADR-0006: Batched Async Storage Trait

## Status

Proposal

## Summary

The storage layer exposes a coarse-grained, batched async API: a `Storage` trait that begins transactions, and a `Txn` handle with `get_many`, resumable `scan` with pushed-down pure filters and projections, buffered `put`/`delete`, explicit read-set registration, savepoints and `commit`. One call per batch of rows, never one per row.

## Context

Every storage call crosses from async code to the synchronous storage pool ([ADR-0001](0001-fjall-storage-engine-and-async-sync-boundary.md)), so per-row calls would be dominated by handoff cost. Scans must be resumable without holding fjall iterators or snapshots across `.await`. Later layers need hooks: the IR executor's leaf scans, Cedar residuals as storage-side filters, policy reads joining the SSI read set, and WASM host functions.

## Decision

```rust
#[async_trait]
pub trait Storage: Send + Sync + 'static {
    async fn begin(&self, opts: TxnOptions) -> Result<Txn>;
}

pub struct TxnOptions {
    pub isolation: Isolation,   // ReadCommitted | RepeatableRead | Serializable
    pub read_only: bool,
}

/// Cheap cloneable handle. Read set, write buffer, statement counter and
/// savepoint stack sit behind one internal lock.
#[derive(Clone)]
pub struct Txn(Arc<TxnInner>);

impl Txn {
    // Reads: go to the storage pool, merged with write-buffer entries
    // from earlier statements.
    pub async fn get_many(&self, keys: Vec<Key>) -> Result<Vec<Option<Bytes>>>;
    pub async fn scan(&self, req: ScanRequest) -> Result<ScanBatch>;

    // Writes: buffered on the async side, tagged with the current
    // statement number, no storage call.
    pub fn put(&self, key: Key, value: Bytes);
    pub fn delete(&self, key: Key);

    // Read-set registration for reads made outside scan/get_many
    // (later: policy data loaded for Cedar).
    pub fn register_read(&self, key: Key);
    pub fn register_range(&self, range: KeyRange);

    pub fn savepoint(&self) -> SavepointId;
    pub fn rollback_to(&self, sp: SavepointId) -> Result<()>;
    pub fn next_statement(&self);   // statement counter++; READ COMMITTED refresh

    // Fails with a clear error if other clones are still alive.
    pub async fn commit(self) -> Result<CommitInfo>;  // commit_ts on success
    pub fn abort(self);
}

pub struct ScanRequest {
    pub range: KeyRange,                       // start/end bounds
    pub reverse: bool,
    pub limit_rows: usize,                     // default 1024
    pub limit_bytes: usize,                    // default 1 MiB
    pub resume: Option<Key>,                   // from the previous ScanBatch
    pub filter: Option<Arc<dyn RowFilter>>,    // runs on the storage thread
    pub projection: Option<Projection>,        // columns to keep
}

pub trait RowFilter: Send + Sync {
    fn eval(&self, key: &[u8], value: &[u8]) -> bool;  // must be pure
}

pub struct ScanBatch {
    pub rows: Vec<(Key, Bytes)>,
    pub resume: Option<Key>,                   // None = range exhausted
}
```

Rules:

- **`Txn` is a shared handle.** Within one query's stream tree, join leaves, index lookups and DML sinks use the same transaction at the same time, which a `&mut self` API can't express. Cloning is cheap, and lock contention is negligible with one task per query. `commit` and `abort` consume the handle, and `commit` fails if any other clone is still alive, so no operator can write after the commit.
- **Resuming needs no held fjall snapshot.** A later call reads at the same `ts`, and newer versions are ignored by timestamp, so each pool call opens and closes its own fjall iterator. Only the GC watermark ([ADR-0008](0008-version-garbage-collection.md)) protects versions at or above the oldest active snapshot.
- **Filtering happens after the read is recorded.** Rows a filter rejects were still read, so the scanned range enters the read set regardless of the filter result; otherwise SSI would miss conflicts.
- **Filters must be pure.** Only leakproof, side-effect-free code may run here. This is where IR predicates and Cedar residuals land later.
- **Write-buffer merge.** Scans merge the transaction's own buffered writes into each batch, so a scan sees its own inserts and deletes. Only entries from earlier statements are merged; the current statement's writes are invisible to its own reads ([ADR-0004](0004-optimistic-transactions-and-isolation-levels.md)).
- **Future batch decoding.** The planned `BatchDecoder` extension ([ADR-0023](0023-leaf-operators-and-storage-pushdown.md)) is defined as a trait in the storage crate. Its Arrow implementation lives in the executor crate, so storage never depends on Arrow.

## Implementation Phases

- [ ] **Phase 1: `Storage` trait, `get_many`, scans with resume, write buffer** - M2
- [ ] **Phase 2: Savepoints, `next_statement`, commit wiring** - M3

## Consequences

### Positive
- Thread-handoff cost amortized per batch.
- No storage resources held across `.await`; cancellation is safe.
- Filters and projections run where the data is, reducing copies.
- Clear hooks for the IR executor (`ScanRequest` as leaf target), Cedar (`register_read`/`register_range`, `RowFilter`) and WASM (host functions map onto `Txn` methods; the async trait lets wasmtime suspend guests).
- The same trait admits a deterministic in-memory implementation for simulation.

### Negative
- Raw `Bytes` rows mean decoding happens on the async side until a batch decoder exists.
- Purity of `RowFilter` is a contract, not enforced by the type system.

## Open Questions

- None.

## Future Work

- `BatchDecoder` extension to `ScanRequest` returning Arrow directly ([ADR-0023](0023-leaf-operators-and-storage-pushdown.md)).
- Network endpoints, pgwire.

## References

- `mvp-fjall-storage-design.md` — Storage trait API; Hooks for later layers
- [ADR-0001: fjall Storage Engine and Async/Sync Boundary](0001-fjall-storage-engine-and-async-sync-boundary.md)
- [ADR-0004: Optimistic Transactions and Isolation Levels](0004-optimistic-transactions-and-isolation-levels.md)
- [ADR-0008: Version Garbage Collection](0008-version-garbage-collection.md)
- [ADR-0018: Policy Barrier and Write Check Plan Constructs](0018-policy-barrier-and-write-check-plan-constructs.md)
- [ADR-0019: Transactional Authorization and Residual Cache](0019-transactional-authorization-and-residual-cache.md)
- [ADR-0023: Leaf Operators and Storage Pushdown](0023-leaf-operators-and-storage-pushdown.md)
- [ADR-0029: WASM UDF Interface](0029-wasm-udf-interface.md)
