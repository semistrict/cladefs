#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Benchmark cladefs against ext4 on virtio-blk (scripts/bench.sh) on the test host, an x86-64 Linux
# machine of one's own with KVM, over ssh (CLADEFS_TEST_HOST; scripts/lib/host.sh),
# as a measurement, alone there and on CPUs of its own (hotbed run --bench), and keep the results in the
# repo: the measurements, the test host, the source measured, the comparison of the filesystems
# (comparison.md) and what the run was for (note.md); and bring
# docs/measurements/fs-bench-history.md up to date (scripts/bench-history.py).
#
#   scripts/bench-host.sh [RESULTS]
#   scripts/bench-host.sh resume RUN RESULTS | cancel RUN
#
# The results go to docs/measurements/HOST-fs-bench-<time>-<mark>, the run's mark
# (scripts/lib/host.sh), unless named. CLADEFS_BENCH_ITERATIONS is how many times each workload runs
# (default: 3). CLADEFS_BENCH_CASES and CLADEFS_BENCH_GROUPS choose what runs (scripts/bench.sh).
# CLADEFS_BENCH_PERF=record profiles each case's VMM with perf, into perf-CASE.txt, record:EVENTS
# records the events there instead, and stat:EVENTS counts the events each 100 ms, into the cases'
# logs (test_fs_bench.rs). CLADEFS_BENCH_GUEST_PERF, if set, profiles the guest's kernel through
# each workload, into the cases' logs. CLADEFS_BENCH_FILES and CLADEFS_BENCH_PGBENCH_SECS and
# CLADEFS_BENCH_PGBENCH_SCALE size the metadata workloads and pgbench, and CLADEFS_BENCH_VCPUS and
# CLADEFS_BENCH_JOBS the VMs' vCPUs and the scaling group's jobs (test_fs_bench.rs).
# CLADEFSD_BUCKET, if set, names the bucket the extent daemon keeps its cells in (a
# bucket's URL the test host reaches), and CLADEFSD_PART_CACHE the directory it keeps what it reads
# from there in. CLADEFS_ZSTD_LEVEL is the zstd level extent stores compress their parts at (default 1; 0
# stores them raw), to measure others. CLADEFS_BUCKET_LATENCY_MS adds that many milliseconds to each
# of the bucket's requests (bucket_cell's object_store_at), as a farther bucket would.
# CLADEFS_GUEST_CMDLINE adds to the guest kernel's arguments (such as cpuidle_haltpoll.force=1), to
# measure them. CLADEFS_GUEST_KERNEL_BUILD (release or debug) builds the guest kernel from the
# source on the test host. CLADEFS_TEST_HOST_LIMIT caps the run's time there (timeout's DURATION,
# default 6h). CLADEFS_BENCH_NOTE says what the run is for. Every other CLADEFS_*, CLADEFSD_* and
# PJDFSTEST_* variable set here reaches the test host too (scripts/lib/host.sh), such as
# CLADEFS_BENCH_WATCHDOG_SECS.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo/scripts/lib/host.sh"
iterations=${CLADEFS_BENCH_ITERATIONS:-3}
[[ "$iterations" =~ ^[1-9][0-9]*$ ]] || { echo 'CLADEFS_BENCH_ITERATIONS is a count' >&2; exit 2; }
limit=6h
# A measurement, alone on the node's measurement CPUs (hotbed run --bench); with
# CLADEFS_TEST_HOST_BENCH=0, a run of work beside others, whose numbers are indicative only, which
# its note says.
host_bench=${CLADEFS_TEST_HOST_BENCH:-1}

# finish STATUS writes what the run was for, brings the history up to date, and says how the run
# went: STATUS, unless the history failed.
finish() {
    local status=$1
    {
        echo "${CLADEFS_BENCH_NOTE:-}"
        echo
        echo "Iterations: $iterations. Cases: ${CLADEFS_BENCH_CASES:-all}." \
            "Workloads: ${CLADEFS_BENCH_GROUPS:-all}.${CLADEFS_BENCH_PERF:+ perf: $CLADEFS_BENCH_PERF.}" \
            "${CLADEFS_BENCH_GUEST_PERF:+Guest kernel profiled.}"
    } > "$results/note.md"
    "$repo/scripts/bench-history.py" >&2 || status=$?
    if [[ -f $results/comparison.md ]]; then cat "$results/comparison.md" >&2; fi
    echo "Results: $results" >&2
    return "$status"
}
host_after=finish
host_actions "$@"
(( $# <= 1 )) || { echo "Usage: $0 [RESULTS] | resume RUN RESULTS | cancel RUN" >&2; exit 2; }
results=${1:-$repo/docs/measurements/$(host_label)-fs-bench-$(date -u +%Y-%m-%dT%H%MZ)-$host_mark}

status=0
host_need_arch x86_64 "bench-host.sh: its cases boot an x86-64 guest kernel"
host_start
echo "Results: $results" >&2
host_job bench-linux-host.sh "$iterations" > "$results/remote.log" 2>&1 || status=$?
if [[ $host_state == ended ]]; then
    host_fetch || status=$?
fi
finish "$status"
