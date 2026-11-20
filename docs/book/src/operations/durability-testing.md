# Testing durability

The normal test suite checks crash recovery with short runs. Three scripts in `scripts/` run the longer checks that durability claims rest on. Each exits non-zero on failure, so CI can run them nightly. The repository has no CI configuration yet (backlog `nightly-durability-ci`).

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
