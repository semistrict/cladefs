#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Rounds of guest test cases in a hotbed workspace on a test host, each case run once a round,
# the cases' order reversed every other round (A B, B A, ...), so that what drifts on the host over
# the hours moves every case alike (TASK-350). The rounds run as one measurement, a `hotbed run
# --bench` command, which has the node's measurement CPUs, most of its L3 and its memory bandwidth
# to itself whatever else the host runs (the fleet's hotbed): what the rest of the host does reaches
# a run only through what isolation can't split, the disks and the kernel's shared locks. Before each
# run it syncs; and it records, in RESULTS/runs.tsv, each run's round, case, outcome, the host's load
# and the ARC's size before and after, and the time the run's tasks stalled (pressure stall
# information of the measurement's own cgroup, in ms, `some`) waiting on CPU, memory and IO: what a
# run's numbers are read against. Nothing runs as root, which a workspace has none of: no cache is
# dropped (board 6058).
#
#   scripts/bench-rounds.sh RESULTS ROUNDS CASE...
#
# The cases' artifacts are built once, first, into RESULTS/artifacts (CLADEFS_E2E_BUILD_ONLY), as
# ordinary work, CARGO_BUILD_JOBS at once (4 unless the caller says), into the caller's
# CARGO_TARGET_DIR if one is set, and every run boots those (CLADEFS_E2E_PREBUILT): no run builds,
# which a measurement refuses to (scripts/lib/measurement.sh, TASK-368), nor runs the harness's own
# tests. CLADEFS_TEST_HOST_PHASE says which: build, a command of work, the artifacts alone; measure,
# the rounds, under hotbed run --bench, of what build made; unset, both, here, for a run of no
# measurement's.
#
# A run's numbers are cladefs's on the workspace's filesystem, ZFS on the fleet's hosts, its cells
# and its extent daemon's part cache in the command's temporary directory (scripts/test-e2e-job.sh),
# its bucket test-e2e.sh's own, a file bucket in memory, on tmpfs, whose distance
# CLADEFS_BUCKET_LATENCY_MS stands for, unless CLADEFSD_BUCKET names one.
#
# Each run's results go to RESULTS/CASE/round-K: test-e2e.sh's logs (e2e, copied there as the run
# ends, without the cells and the guests' root filesystems), and the fs bench's report as JSON
# (CLADEFS_BENCH_RESULTS), which scripts/bench-ab.py compares, a side's directories its cases'
# round directories; and what others wrote to the run's disk meanwhile (contamination.json there,
# scripts/contamination.py, TASK-375), the cells', which test-e2e-job.sh names, a run others wrote
# beside left out of the comparison's timings. The environment's CLADEFS_* and CLADEFSD_* variables
# pass to every run: CLADEFS_GUEST_KERNEL, CLADEFS_BENCH_ITERATIONS, CLADEFS_BENCH_GROUPS,
# CLADEFS_PGBENCH_RUNS and such. It fails if a run did; a run refused before it started (2:
# artifacts it can't boot) ends the rounds there, as every other would be. CLADEFS_TEST_HOST_PROC
# and CLADEFS_TEST_HOST_SYS name the host's /proc and /sys, for the tests.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
(( $# >= 3 )) || { sed -n '/^#   scripts/p' "$0" >&2; exit 2; }
results=$1 rounds=$2
shift 2
cases=("$@")
[[ $rounds =~ ^[1-9][0-9]*$ ]] || { echo "ROUNDS is a count" >&2; exit 2; }
mkdir -p "$results"
results=$(cd "$results" && pwd)
phase=${CLADEFS_TEST_HOST_PHASE:-all}
[[ $phase == all || $phase == build || $phase == measure ]] ||
    { echo "CLADEFS_TEST_HOST_PHASE is build or measure, or unset for both" >&2; exit 2; }
proc=${CLADEFS_TEST_HOST_PROC:-/proc} sys=${CLADEFS_TEST_HOST_SYS:-/sys}
load() { cut -d' ' -f1 "$proc/loadavg"; }
arc_bytes() {
    awk '$1 == "size" { print $3 }' "$proc/spl/kstat/zfs/arcstats" 2> /dev/null || echo -
}
# The time, in µs, this command's cgroup's tasks have stalled on RESOURCE (cpu, memory, io), some
# of them at least: the measurement's own, hotbed's unit of it.
cgroup=$sys/fs/cgroup$(sed -n 's/^0:://p' "$proc/self/cgroup")
stalled() {
    sed -n 's/^some .*total=//p' "$cgroup/$1.pressure"
}
artifacts=$results/artifacts
if [[ $phase != measure ]]; then
    source "$repo/scripts/lib/measurement.sh"
    measurement_refuses "bench-rounds.sh's build"
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} CLADEFS_E2E_BUILD_ONLY=$artifacts \
        "$repo/scripts/test-e2e.sh" "${cases[@]}" > "$results/build.log" 2>&1 ||
        { echo "building failed: $results/build.log" >&2; exit 1; }
fi
if [[ $phase == build ]]; then exit 0; fi
[[ -f $results/runs.tsv ]] ||
    printf 'round\tcase\toutcome\tload_before\tload_after\tarc_before\tarc_after\tstarted\t%s\n' \
        'cpu_stall_ms	memory_stall_ms	io_stall_ms' \
        > "$results/runs.tsv"
status=0
for ((round = 1; round <= rounds; round++)); do
    order=("${cases[@]}")
    if (( round % 2 == 0 )); then
        order=()
        for ((i = ${#cases[@]} - 1; i >= 0; i--)); do order+=("${cases[i]}"); done
    fi
    for case in "${order[@]}"; do
        out=$results/$case/round-$round
        mkdir -p "$out"
        sync
        load_before=$(load) arc_before=$(arc_bytes) started=$(date -u +%FT%TZ)
        stalls_before=()
        for resource in cpu memory io; do stalls_before+=("$(stalled "$resource")"); done
        outcome=ok ran=0
        CLADEFS_E2E_PREBUILT=$artifacts CLADEFS_BENCH_RESULTS=$out \
            "$repo/scripts/test-e2e-job.sh" "$out/e2e" "$case" > "$out/run.log" 2>&1 || ran=$?
        if (( ran == 2 )); then
            echo "round $round $case: refused, and so would every run be: $out/run.log" >&2
            exit 2
        fi
        if (( ran )); then outcome=failed; fi
        load_after=$(load)
        stalls=()
        i=0
        for resource in cpu memory io; do
            stalls+=($(( ($(stalled "$resource") - stalls_before[i]) / 1000 )))
            i=$((i + 1))
        done
        [[ $outcome == ok ]] || status=1
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$round" "$case" "$outcome" \
            "$load_before" "$load_after" "$arc_before" "$(arc_bytes)" "$started" "${stalls[@]}" \
            >> "$results/runs.tsv"
        echo "round $round $case: $outcome (stalled ms: cpu ${stalls[0]}, memory ${stalls[1]}, io ${stalls[2]})" >&2
    done
done
exit "$status"
