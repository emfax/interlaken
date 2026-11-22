# Testing durability

The normal test suite checks crash recovery with short runs. Three scripts in `scripts/` run the longer checks that durability claims rest on. Each exits non-zero on failure. The nightly workflow, `.github/workflows/nightly.yml`, runs `power-loss.sh` and `disk-space-soak.sh` on `main` with their defaults, together with the full test suite and the slow ignored tests, on nights after new commits.

## Power loss: `scripts/power-loss.sh`

```sh
scripts/power-loss.sh                 # every crash test, 50 cycles each
CRASH_CYCLES=200 scripts/power-loss.sh
```

This runs the crash harness, `crates/storage/tests/crash.rs`, on [LazyFS](https://github.com/dsrhaslab/lazyfs), a FUSE filesystem that can drop writes that weren't synced. Each cycle runs a workload that commits continuously, kills it, and then either restarts it (a process crash) or first discards every unsynced write (a power loss). It then checks the reopened database:

| Test | Requirement |
| --- | --- |
| process crash, `Sync` and `Buffered` | no acknowledged commit lost |
| power loss, `Sync` | no acknowledged commit lost |
| power loss, `Buffered` | losses only as a suffix of the commit order, never part of a transaction |

The workload uses 1 MiB memtables, so power loss also strikes memtable flushes and journal rotation.

The script needs FUSE (`/dev/fuse` and `fusermount3`) but not root. It builds LazyFS with `scripts/build-lazyfs.sh` if `LAZYFS_ROOT` doesn't point at a build, which needs git, CMake 3.16 or later, a C++17 compiler, pkg-config and network access. `LAZYFS_CACHE` sets LazyFS's page cache size (512 MB by default).

Without `LAZYFS_ROOT`, `cargo test` skips the power-loss tests and runs only the process-crash ones.

## Both commit paths

Commits take one of two paths through storage: the single-node sequencer, by default, or the commit log, which replication will build on ([ADR-0037](../reference/design-records.md)). `StorageOptions::commit_log` chooses between them, and its default follows the environment variable `INTERLAKEN_COMMIT_LOG`. So any test or script runs on the log path with that variable set to `1`:

```sh
INTERLAKEN_COMMIT_LOG=1 cargo test --release -p interlaken-storage
INTERLAKEN_COMMIT_LOG=1 scripts/power-loss.sh
```

Pull requests run the storage tests on both paths. The nightly workflow runs the whole test suite and the power-loss tests on both.

## Replicated clusters under faults

`interlaken-replication` replicates storage with Raft ([ADR-0037](../reference/design-records.md)). Its simulation tests run clusters of three and five replicas inside [turmoil](https://github.com/tokio-rs/turmoil), which simulates the network and time between them. While clients commit on every replica, a schedule drawn from a seed cuts minorities off, heals partitions, and crashes and restarts replicas from their directories. Replicas snapshot every 30 entries, so those that fall behind catch up from snapshots. One workload also adds a new replica and has it replace another while the faults go on:

```sh
SIM_SEEDS=50 cargo test --release -p interlaken-replication --test simulation
SIM_SEED=17 cargo test --release -p interlaken-replication --test simulation
```

`SIM_SEEDS` sets how many seeds each test runs (two by default), and `SIM_SEED` runs one. Every run checks that:
- Elle finds list-append histories strictly serializable when every snapshot is fresh, and strongly session-serializable when read-only transactions take local snapshots;
- no acknowledged commit is lost;
- a commit whose outcome was unknown happened at most once;
- the bank workload's total never changes;
- after healing, every replica holds the same data.

Elle needs Java and `elle-cli`, as the storage crate's Elle tests do; without them the other checks still run, and histories are saved under `target/elle/`.

Storage runs on real threads, so a seed usually reproduces a failure but isn't guaranteed to. Failures report their seed.

## Disk space: `scripts/disk-space-soak.sh`

```sh
SOAK_SECS=1800 TMPDIR=/data/tmp scripts/disk-space-soak.sh
```

This overwrites a fixed set of keys at a steady rate with GC running, samples `disk_space_bytes` every second, and fails if the later half of the run peaks more than 25% (plus 8 MiB) above the earlier half. Settings:

| Variable | Default | Meaning |
| --- | --- | --- |
| `SOAK_SECS` | 600 | run length in seconds |
| `SOAK_MIB_PER_SEC` | 16 | commit rate, in MiB/s of keys and values |
| `SOAK_JOURNAL_MIB` | 512 | the journal cap |
| `SOAK_MEMTABLE_MIB` | 64 | the memtable size |
| `TMPDIR` | system default | where the database lives; put it on the disk to measure |

## Commit stalls: `scripts/commit-stalls.sh`

```sh
scripts/commit-stalls.sh /mnt/volume/tmp
```

`DIR` must be on the disk to measure, not tmpfs. The script measures two things:
- **Raw fsync latency:** 4 KiB appends, each synced. `FSYNC_PROBES` sets the number, 2,000 by default.
- **Commit latency:** the commit benchmark under `Durability::Sync`, for `BENCH_SECS` per run, 10 by default.

Both report p50, p99 and maximum latencies. Run it on the hardware you deploy to: occasional sync stalls of hundreds of milliseconds were seen on one development disk, and commits under `Sync` wait for them.
