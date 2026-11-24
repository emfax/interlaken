# Configuration

An engine is configured when it is created or opened, through `EngineOptions` passed to `Engine::create` or `Engine::open`. `EngineOptions` includes the storage settings, `StorageOptions`. Start from the defaults and change only what you need:

```rust
use interlaken_engine::EngineOptions;
use interlaken_storage::{Durability, StorageOptions};

let options = EngineOptions {
    node_memory_limit: Some(1536 << 20),
    storage: StorageOptions {
        durability: Durability::Sync,
        ..StorageOptions::default()
    },
    ..EngineOptions::default()
};
```

Some settings are fixed when the database is created: the policy settings, and the memtable size, which later opens keep. The [Memory](memory.md), [Durability](durability.md) and [Garbage collection](gc.md) chapters explain how to choose the settings that matter most.

This reference is generated from the options structs' documentation and their `Default` implementations, and a test fails if it falls out of date.

<!-- begin generated: options -->
## `EngineOptions`

### `storage`

Type `StorageOptions`. Default: `StorageOptions::default()` (below).

Storage settings.

### `query_budget`

Type `usize`. Default: 256 MiB.

Each statement's memory budget in bytes (ADR-0027).

### `node_memory_limit`

Type `Option<usize>`. Default: none.

A node-wide cap on query memory shared by all sessions, if any.

### `residual_cache_capacity`

Type `usize`. Default: 4,096.

How many residuals the residual cache holds (ADR-0019).

### `policy_settings`

Type `PolicySettings`. Default: mode `analysis-sufficient`, application context `{}`.

The policy settings a new database starts with (`Engine::create` only).

### `attribution`

Type `Attribution`. Default: `Attribution::Off`.

What labels the `interlaken_usage_*` metrics, if anything (ADR-0039).

### `attribution_limit`

Type `usize`. Default: 1,000.

The most distinct attribution keys; sessions with new keys beyond
it count as `other`, which bounds the metrics' series.

## `StorageOptions`

### `pool_threads`

Type `usize`.

Storage pool threads. Defaults to the number of cores, at least 2.

### `queue_capacity`

Type `usize`.

Bounded storage pool queue length. Defaults to 1,024.

### `durability`

Type `Durability`.

When commits are acknowledged. Defaults to `Durability::Sync`.

### `max_commit_group`

Type `usize`.

Most transactions the sequencer writes in one group. Defaults to 256.

### `commit_queue_capacity`

Type `usize`.

Bounded commit queue length. Defaults to 1,024.

### `max_write_set_bytes`

Type `usize`.

Most bytes of keys plus values one transaction may buffer. Defaults to 64 MiB.

### `max_transaction_age`

Type `Duration`.

Longest a transaction may stay open. Defaults to 5 minutes.

An older transaction fails its next operation with
`Error::TransactionTooOld` and no
longer holds back version GC (ADR-0004, ADR-0008). `Duration::MAX`
disables the limit.

### `gc_interval`

Type `Option<Duration>`.

How often the background GC pass runs, or `None` to run GC only
through `FjallStorage::run_gc`. Defaults to 10 seconds.

### `gc_chunk_versions`

Type `usize`.

Versions one GC chunk examines before yielding the storage thread.
Defaults to 4,096.

### `max_journal_bytes`

Type `u64`.

Most bytes of journal fjall keeps before flushing memtables so the
oldest journal can be deleted. Defaults to 512 MiB; values below
64 MiB are raised to 64 MiB.

Under sustained writes the journals dominate the on-disk footprint,
which saws between roughly the live data and this cap plus the
memtables (ADR-0008, *As implemented (F4)*).

### `memtable_bytes`

Type `u64`.

Size at which a keyspace's memtable is flushed to a table. Defaults
to 64 MiB; values below 1 MiB are raised to 1 MiB. Recorded when the
database is created; later opens keep the original size.

### `commit_log`

Type `bool`.

Commit through the commit log (ADR-0037 phase 1): each validated
group is first appended to the `raftlog` keyspace, which is the
commit, and then applied to the data by a separate thread, as
replication will need. `false` is the single-node path. Defaults to
`false`, or `true` when `INTERLAKEN_COMMIT_LOG` is `1`; the
environment variable lets every test suite run on either path unchanged.

A database can switch between the two paths from one open to the
next: opening it applies anything left in the log first.

### `metrics_interval`

Type `Option<Duration>`.

How often the metrics sampler records sizes, backlogs and
transaction counts as gauges, or `None` for no sampler (ADR-0039).
Defaults to 10 seconds. With no metrics recorder installed, each
sample reads a few of fjall's counters and is discarded.

<!-- end generated: options -->
