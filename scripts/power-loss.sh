#!/usr/bin/env bash
# Run the crash harness with injected power loss on LazyFS (ADR-0009 phase 7).
#
# Usage: scripts/power-loss.sh [extra cargo test args, e.g. a test-name filter]
#
# Environment:
#   LAZYFS_ROOT   a built LazyFS checkout (default ~/.cache/interlaken/lazyfs;
#                 built with scripts/build-lazyfs.sh if missing)
#   CRASH_CYCLES  kill/power-loss cycles per test (default 50 here, 6 in cargo test)
#   LAZYFS_CACHE  LazyFS page cache size (default 512mb)
#
# Runs every test in crates/storage/tests/crash.rs: process crashes under
# Sync and Buffered, and power loss under Sync (no acknowledged commit may be
# lost) and Buffered (losses allowed, only as a suffix of the commit order,
# never a torn transaction). Exits non-zero on any failure, so CI can run it
# nightly. Needs FUSE (/dev/fuse, fusermount3) but not root.
set -euo pipefail

cd "$(dirname "$0")/.."
export LAZYFS_ROOT="${LAZYFS_ROOT:-$HOME/.cache/interlaken/lazyfs}"
if [ ! -x "$LAZYFS_ROOT/lazyfs/build/lazyfs" ]; then
    scripts/build-lazyfs.sh "$LAZYFS_ROOT"
fi
export CRASH_CYCLES="${CRASH_CYCLES:-50}"

# One test at a time: each mounts its own LazyFS and the cycles are timing-based.
cargo test --release -p interlaken-storage --test crash "$@" -- --nocapture --test-threads=1
