# Garbage collection and disk space

## Versions

Storage is multi-versioned. An update or delete doesn't overwrite a row, but writes a new version and keeps the old one for transactions whose snapshots still need it. A background *garbage collector* removes versions that no snapshot can see any more. Its watermark is the oldest snapshot an open transaction holds:

| Setting | Default | Effect |
| --- | --- | --- |
| `StorageOptions::gc_interval` | 10 seconds | how often a GC pass runs; `None` disables the background task, and `FjallStorage::run_gc()` runs a pass on demand |
| `gc_chunk_versions` | 4,096 | versions one GC step examines before yielding the storage thread, so GC doesn't delay queries |
| `max_transaction_age` | 5 minutes | the longest a transaction may stay open |

A long-running transaction holds back the watermark, and every version newer than its snapshot is kept for as long as it is open. `max_transaction_age` bounds that. A transaction older than the limit fails its next operation with `25P04`, and stops holding back GC. Keep transactions short, and use read-only transactions for long reports: they still hold a snapshot, so they too are bounded by the age limit.

## Disk space

Removed versions free their space only when fjall compacts the files that held them, so disk use lags GC. Under sustained writes, the on-disk footprint is dominated by fjall's journals rather than by old versions:
- journals are deleted only once the memtables they cover are flushed to tables;
- so the footprint rises and falls between about the size of the live data and that plus the journal cap and the memtables.

Two settings bound it:

| Setting | Default | Minimum | Effect |
| --- | --- | --- | --- |
| `StorageOptions::max_journal_bytes` | 512 MiB | 64 MiB | journal bytes kept before memtables are flushed so the oldest journal can be deleted |
| `memtable_bytes` | 64 MiB | 1 MiB | the size at which a keyspace's memtable is flushed; fixed when the database is created, and later opens keep the original size |

A sustained 16 MiB/s update workload with the defaults wrote 4.8 GiB in 300 seconds and used between 80 and 696 MiB of disk, with no upward trend. With a 64 MiB journal cap and 4 MiB memtables, a smaller workload levelled off at 70–160 MiB. An embedded deployment that cares about its disk footprint should lower both settings. The cost is more frequent, smaller flushes.

`StorageMetrics::disk_space_bytes` reports the space in use (see [Metrics and observability](metrics.md)), and `scripts/disk-space-soak.sh` checks that it levels off under a sustained workload (see [Testing durability](durability-testing.md)).

A dropped table's rows and indexes are deleted in the same transaction as the drop, and their space returns the same way.
