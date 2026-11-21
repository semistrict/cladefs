#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Benchmark cladefs against ext4 on virtio-blk (scripts/bench.sh) on an x86-64
# Linux KVM host, as root (scripts/bench-host.sh drives it), writing the
# measurements, the guests' logs and the comparison to RESULTS, and what others
# wrote to its disk meanwhile (RESULTS/contamination.json,
# scripts/contamination.py, TASK-375): a run others wrote beside says so, in
# bench.log's last lines and host.txt.
#
# Usage: bench-linux-host.sh SOURCE RESULTS [ITERATIONS]
#
# CLADEFS_BENCH_CASES and CLADEFS_BENCH_GROUPS choose what runs (scripts/bench.sh).
# CLADEFS_GUEST_KERNEL_BUILD (release or debug) builds the guest kernel from SOURCE.
# CLADEFS_TEST_HOST_PHASE (linux-host-setup.sh's host_phase) is what of it runs: `build` builds the
# cases' artifacts and the guest kernel into the work directory, as work; `measure` runs the
# bench against them, building nothing; unset, both (scripts/bench-host.sh runs the two as jobs
# of their own, TASK-368).
set -euo pipefail
repo=$(cd "${1:?source required}" && pwd)
results=${2:?results directory required}
iterations=${3:-3}
source "$(dirname "${BASH_SOURCE[0]}")/linux-host-setup.sh"
host_setup
host_tree bench-tree
artifacts=$work/artifacts-bench
if [[ $host_phase != measure ]]; then
    # Build the guest kernel from this source when asked, as test-linux-host.sh does; or boot
    # CLADEFS_GUEST_KERNEL's.
    kernel=${CLADEFS_GUEST_KERNEL:-}
    case ${CLADEFS_GUEST_KERNEL_BUILD:-} in
        '') ;;
        release | debug)
            host_kernel "$CLADEFS_GUEST_KERNEL_BUILD"
            kernel=$host_kernel_image
            ;;
        *) echo "CLADEFS_GUEST_KERNEL_BUILD is release or debug" >&2; exit 2 ;;
    esac
    host_artifacts "$artifacts" bench "$kernel"
    if [[ $host_phase == build ]]; then
        echo "Built in ${SECONDS}s." | tee "$results/build.txt"
        exit 0
    fi
fi
host_prebuilt "$artifacts"
if [[ -n ${CLADEFS_GUEST_KERNEL:-} ]]; then
    sha256sum "$CLADEFS_GUEST_KERNEL" > "$results/guest-kernel.txt"
fi
# With CLADEFS_BENCH_PERF=record, perf samples each case's VMM, its threads'
# stacks (record-all: the whole host's, the extent daemon's too), and
# perf-CASE.txt says where their time went, and perf-CASE-THREAD.txt each busy
# thread's (scripts/lib/perf-report.sh), or with record:EVENTS
# where the events happened; with stat:EVENTS, it counts the events into the
# cases' logs (test_fs_bench.rs).
if [[ -n ${CLADEFS_BENCH_PERF:-} ]]; then
    host_need perf
fi

{
    host_record
    findmnt -no SOURCE,FSTYPE,OPTIONS /
    podman --version
} > "$results/host.txt"

status=0
CLADEFS_BENCH_RESULTS="$results" "$repo/scripts/contamination.py" run "$results" -- \
    "$repo/scripts/bench.sh" "$iterations" > "$results/comparison.md" 2> "$results/bench.log" ||
    status=$?
echo "Others' I/O beside it: $("$repo/scripts/contamination.py" said "$results")" |
    tee -a "$results/host.txt"
source "$(dirname "${BASH_SOURCE[0]}")/perf-report.sh"
for data in "$results"/logs/*/*/perf.data; do
    [[ -f $data ]] || continue
    case=$(basename "$(dirname "$data")")
    perf_report "$data" "$results/perf-$case" || status=$?
    rm -f "$data"
done
# The guests' root filesystems, copies of the Alpine image (and their images,
# under Firecracker), and the filesystems' images and cells, which a failed
# case leaves, are not results.
find "$results/logs" -type d \( -name rootfs -o -name extents -o -name '*.img' \) -prune \
    -exec rm -rf {} +
find "$results/logs" -type f \( -name '*.img' -o -name rootfs.ext4 \) -delete
echo "Took ${SECONDS}s." | tee -a "$results/host.txt"
cat "$results/comparison.md"
exit "$status"
