# Durability

`StorageOptions::durability` decides when a commit is acknowledged, which decides what a crash can lose:

| Mode | A commit is acknowledged | Survives a process crash | Survives power loss |
| --- | --- | --- | --- |
| `Durability::Sync` (the default) | after the journal is synced to stable storage | yes | yes |
| `Durability::Buffered` | once the write reaches the operating system's buffers | yes | no: recent acknowledged commits may be lost |

Use `Sync` for anything whose data matters, and `Buffered` for development, tests and data you can rebuild.

Losses under `Buffered` are always a suffix of the commit order: the most recent commits, whole. A crash never tears a transaction, never keeps a later commit while losing an earlier one, and never leaves the database unable to open. Testing on a fault-injecting filesystem that drops unsynced writes confirmed this. Over 119,467 acknowledged commits, `Sync` lost none. `Buffered` lost recent commits, always as a suffix, never part of a transaction (see [Testing durability](durability-testing.md)).

## Group commit

One commit thread validates and writes commits in groups. Each group is one write and, under `Sync`, one sync, however many transactions it holds. Concurrent committers share the cost of the sync, so throughput grows with concurrency. Measured on a development machine with an NVMe disk, for one write per transaction:

| Durability | 1 client | 16 clients | 256 clients |
| --- | --- | --- | --- |
| `Sync` | 4,100 commits/s | 26,600 commits/s | 225,000 commits/s |
| `Buffered` | 95,000 commits/s | 306,000 commits/s | 307,000 commits/s |

Under `Sync`, a commit's latency is dominated by the disk's sync latency. Disks occasionally stall a sync for hundreds of milliseconds, and commits wait with them; `scripts/commit-stalls.sh` measures this on a given disk (see [Testing durability](durability-testing.md)). `StorageOptions::max_commit_group` caps a group's size, and `commit_queue_capacity` caps the commits waiting to join one.

## When a commit's outcome is unknown

If writing a commit group fails, for example because the disk fails or fills up, the commits in it fail with an error saying their outcome is unknown. They may or may not be durable. The commit thread then stops, and every later commit fails until the database is reopened. Reopening recovers to a consistent state: each commit in that group is either fully there or fully absent. An application that needs to know must check after reopening.

## Recovery

Opening a database after a crash replays fjall's journal and resumes the commit clock after the last durable commit. No separate recovery step is needed. The on-disk format isn't stable between Interlaken versions yet: a database written by an incompatible version fails to open with a format version error rather than being misread.
