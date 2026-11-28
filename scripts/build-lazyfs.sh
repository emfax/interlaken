#!/usr/bin/env bash
# Build LazyFS (https://github.com/dsrhaslab/lazyfs) without root, for the
# power-loss tests (ADR-0009 phase 7, scripts/power-loss.sh).
#
# Usage: scripts/build-lazyfs.sh [DIR]
#
# DIR defaults to $LAZYFS_ROOT, then ~/.cache/interlaken/lazyfs. The script
# clones LazyFS there (checking out LAZYFS_REV if set), builds libpcache and
# lazyfs with CMake as LazyFS's own build.sh scripts do, and prints the
# LAZYFS_ROOT to export.
#
# Needs git, cmake (3.16+), g++ (C++17), pkg-config, network access for
# CMake's FetchContent, and the FUSE 3 runtime (libfuse3.so.3, fusermount3,
# /dev/fuse). If the FUSE 3 *headers* are missing (no libfuse3-dev), it
# fetches the libfuse3-dev .deb with `apt-get download` (no root) and
# extracts it inside the checkout. On non-Debian systems install the headers
# (fuse3-devel or similar) or point CPATH and LIBRARY_PATH at them.
set -euo pipefail

dir="${1:-${LAZYFS_ROOT:-$HOME/.cache/interlaken/lazyfs}}"
repo="${LAZYFS_REPO:-https://github.com/dsrhaslab/lazyfs.git}"

for tool in git cmake g++ pkg-config; do
    command -v "$tool" >/dev/null || { echo "error: $tool not found" >&2; exit 1; }
done
[ -e /dev/fuse ] || { echo "error: /dev/fuse missing; FUSE is not available" >&2; exit 1; }
command -v fusermount3 >/dev/null || { echo "error: fusermount3 not found (package fuse3)" >&2; exit 1; }

if [ ! -d "$dir/.git" ]; then
    mkdir -p "$(dirname "$dir")"
    git clone "$repo" "$dir"
fi
if [ -n "${LAZYFS_REV:-}" ]; then
    git -C "$dir" fetch --depth 1 origin "$LAZYFS_REV"
    git -C "$dir" checkout --detach FETCH_HEAD
fi

if ! pkg-config --exists fuse3 && [ ! -f /usr/include/fuse3/fuse.h ]; then
    headers="$dir/.fuse3-dev"
    if [ ! -f "$headers/usr/include/fuse3/fuse.h" ]; then
        command -v apt-get >/dev/null || {
            echo "error: FUSE 3 headers missing and apt-get unavailable; install libfuse3-dev or equivalent" >&2
            exit 1
        }
        echo "FUSE 3 headers missing; fetching libfuse3-dev with apt-get download (no root)"
        tmp="$(mktemp -d)"
        (cd "$tmp" && apt-get download libfuse3-dev)
        dpkg-deb -x "$tmp"/libfuse3-dev_*.deb "$headers"
        rm -rf "$tmp"
    fi
    multiarch="$(gcc -print-multiarch 2>/dev/null || echo x86_64-linux-gnu)"
    export CPATH="$headers/usr/include${CPATH:+:$CPATH}"
    export LIBRARY_PATH="$headers/usr/lib/$multiarch${LIBRARY_PATH:+:$LIBRARY_PATH}"
fi

# CMake 4 refuses projects declaring compatibility with CMake < 3.5, as
# LazyFS's FetchContent dependency toml11 does; this lets them configure.
# Older CMake ignores it.
export CMAKE_POLICY_VERSION_MINIMUM="${CMAKE_POLICY_VERSION_MINIMUM:-3.5}"

jobs="${JOBS:-4}"
(cd "$dir/libs/libpcache" && mkdir -p build && cd build && cmake .. >/dev/null && cmake --build . -j "$jobs")
(cd "$dir/lazyfs" && mkdir -p build && cd build && cmake -DLAZYFS_BUILD_TESTS=OFF .. >/dev/null && cmake --build . -j "$jobs")

echo
echo "LazyFS built: $dir/lazyfs/build/lazyfs"
echo "export LAZYFS_ROOT=$dir"
