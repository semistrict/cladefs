#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run the benchmarks of exact counts on TREE, a cladefs tree, into OUT (TASK-441.1): what a load
# beside them doesn't move, so they run as ordinary work, with a side's build
# (scripts/lib/bench-ab-linux-host.sh's CLADEFS_AB_COUNTS), never in a measurement, which this
# refuses (scripts/lib/measurement.sh), as it builds. scripts/ab-rounds.py compares two sides'.
#
#   scripts/counts.sh TREE OUT WORK
#
# - lifecycle: what a sandbox's life asks of its bucket, by phase (TASK-443.5,
#   `bucket::lifecycle::bucket_lifecycle_by_phase`): requests by kind and bytes, on bases of
#   CLADEFS_COUNTS_LIFECYCLE_EXTENTS extents (1024,32768: 262,144 takes 6 minutes);
# - fork-memory: the heap an open, idle fork holds, by its base's extents (TASK-443.2,
#   `bucket::tests::bench_fork_extent_memory`), counted by the test binary's allocator, at
#   CLADEFS_COUNTS_FORK_EXTENTS extents (1024,32768) and CLADEFS_COUNTS_FORKS forks (20: forks of
#   a version cost the same from the first, 100 of 300,000 extents would hold 12 GB);
# - gc-deleted and gc-clones: the requests cladefs-gc's sweeps make of 100 dead cells, and of 100
#   deleted clones of a base, and its passes over that base (TASK-443.4, scripts/gc-sweep.sh,
#   this tree's, building TREE's cladefs-gc into WORK).
#
# Each benchmark's output goes to OUT/NAME.out (gc-sweep.sh's tables to OUT/NAME/), and its exit
# status to a line `NAME STATUS` of OUT/status.txt: one that failed, or that TREE has not (an older
# commit), is a result of its own, and the others still run. The nextest benchmarks build TREE's
# cladefs tests in the sim profile (optimized, with debug assertions: cladefs's tests refuse a
# release build, TASK-320), in TREE's own target directory. It fails only if it could not start.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo/scripts/lib/measurement.sh"
(( $# == 3 )) || { sed -n '/^#   scripts/p' "$0" >&2; exit 2; }
tree=$(cd "$1" && pwd)
mkdir -p "$2" "$3"
out=$(cd "$2" && pwd)
work=$(cd "$3" && pwd)
measurement_refuses "scripts/counts.sh's builds"
: > "$out/status.txt"

# step NAME COMMAND... runs COMMAND, its output OUT/NAME.out, its status OUT/status.txt's.
step() {
    local name=$1 status=0
    shift
    echo "== $name" >&2
    "$@" > "$out/$name.out" 2>&1 || status=$?
    echo "$name $status" >> "$out/status.txt"
}

# nextest TEST runs TREE's ignored cladefs test TEST, its output not captured: its lines are the
# counts.
nextest() {
    (cd "$tree" && cargo nextest run -p cladefs --cargo-profile sim --run-ignored only \
        --no-capture "$1")
}

lifecycle() {
    CLADEFS_LIFECYCLE_EXTENTS=${CLADEFS_COUNTS_LIFECYCLE_EXTENTS:-1024,32768} \
        nextest bucket_lifecycle_by_phase
}

fork_memory() {
    CLADEFS_BENCH_EXTENTS=${CLADEFS_COUNTS_FORK_EXTENTS:-1024,32768} \
        CLADEFS_BENCH_FORKS=${CLADEFS_COUNTS_FORKS:-20} nextest bench_fork_extent_memory
}

# gc NAME FILL [CELL] sweeps three times a bucket of 100 cells filled as FILL says, and passes over
# CELL alone, if named, into OUT/NAME.
gc() {
    CLADEFS_GC_TENANTS=1 CLADEFS_GC_CELLS=100 CLADEFS_GC_SEGMENTS=4 CLADEFS_GC_LARGEST=16 \
        CLADEFS_GC_FILL=$2 CLADEFS_GC_AT_ONCE=16 CLADEFS_GC_SWEEPS=3 CLADEFS_GC_ROUNDS=1 \
        CLADEFS_GC_CELL=${3:-} "$repo/scripts/gc-sweep.sh" "$work/$1" "$out/$1" "this=$tree"
}

step lifecycle lifecycle
step fork-memory fork_memory
step gc-deleted gc gc-deleted --deleted
step gc-clones gc gc-clones "--clones 1024" 1/1
