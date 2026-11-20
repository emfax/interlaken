#!/usr/bin/env bash
# Measure commit latency stalls and raw fsync stalls on a target disk
# (backlog: fsync-stalls-other-hardware; ADR-0005 "Measured").
#
# Usage: scripts/commit-stalls.sh DIR
#
# DIR must be on the storage to measure (not tmpfs). The script runs
#   1. a raw fsync probe: FSYNC_PROBES (default 2000) 4 KiB appends to one
#      file, each followed by fsync, reporting p50/p99/p99.9/max latency;
#   2. the commit throughput benchmark under Sync (BENCH_SECS per run,
#      default 10), whose rows include p50, p99 and max commit latency.
# Record the host, disk, p99 and max figures in ADR-0005. Non-blocking for
# the MVP; run it once the deployment hardware is known.
set -euo pipefail

dir="${1:?usage: scripts/commit-stalls.sh DIR}"
mkdir -p "$dir"
cd "$(dirname "$0")/.."

echo "== host"
uname -a
df -hT "$dir" | tail -1

echo
echo "== raw fsync probe ($dir)"
python3 -I - "$dir" "${FSYNC_PROBES:-2000}" <<'PY'
import os, sys, time
path = os.path.join(sys.argv[1], "fsync-probe.dat")
n = int(sys.argv[2])
block = os.urandom(4096)
fd = os.open(path, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
lat = []
try:
    for _ in range(n):
        os.write(fd, block)
        t = time.perf_counter()
        os.fsync(fd)
        lat.append(time.perf_counter() - t)
finally:
    os.close(fd)
    os.unlink(path)
lat.sort()
q = lambda p: lat[min(len(lat) - 1, round((len(lat) - 1) * p))] * 1e6
print(f"fsync x{n}: p50 {q(0.5):.0f} us  p99 {q(0.99):.0f} us  "
      f"p99.9 {q(0.999):.0f} us  max {lat[-1] * 1e6:.0f} us")
PY

echo
echo "== commit latency under Sync ($dir)"
BENCH_DIR="$dir" BENCH_SECS="${BENCH_SECS:-10}" \
    cargo bench -p interlaken-storage --bench commit_throughput -- sync
