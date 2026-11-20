#!/usr/bin/env bash
# Run the long disk-space soak (ADR-0009 phase 7): sustained updates with
# version GC, asserting StorageMetrics::disk_space_bytes levels off.
#
# Usage: scripts/disk-space-soak.sh
#
# Environment:
#   SOAK_SECS          run length in seconds (default 600)
#   SOAK_MIB_PER_SEC   target commit rate in MiB/s of keys and values (default 16)
#   SOAK_JOURNAL_MIB   fjall journal cap (default 512, fjall's default; min 64)
#   SOAK_MEMTABLE_MIB  memtable size (default 64)
#   TMPDIR             where the database lives; put it on the disk to measure
#
# Prints one sample per second and a summary line. Under sustained writes the
# footprint saws between the live data and roughly the journal cap plus the
# memtables; the test fails if the later half of the run peaks more than 25%
# (+8 MiB) above the earlier half. Exits non-zero on failure, for nightly CI.
set -euo pipefail

cd "$(dirname "$0")/.."
export SOAK_SECS="${SOAK_SECS:-600}"
cargo test --release -p interlaken-storage --test disk_space \
    disk_space_levels_off_under_sustained_updates -- --ignored --nocapture
