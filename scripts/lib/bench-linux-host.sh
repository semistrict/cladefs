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
set -euo pipefail
repo=$(cd "${1:?source required}" && pwd)
results=${2:?results directory required}
iterations=${3:-3}
source "$(dirname "${BASH_SOURCE[0]}")/linux-host-setup.sh"
host_setup
host_tree bench-tree
# scripts/bench.sh runs its cases through scripts/test-e2e.sh, which runs the VMM's tests under
# nextest first (TASK-220): without it, every run failed there, "no such command: nextest" (GCE r1022).
host_need cargo-nextest
# Build the guest kernel from this source when asked, as test-linux-host.sh does.
case ${CLADEFS_GUEST_KERNEL_BUILD:-} in
    '') ;;
    release | debug)
        host_kernel "$CLADEFS_GUEST_KERNEL_BUILD"
        export CLADEFS_GUEST_KERNEL=$host_kernel_image
        sha256sum "$CLADEFS_GUEST_KERNEL" > "$results/guest-kernel.txt"
        ;;
    *) echo "CLADEFS_GUEST_KERNEL_BUILD is release or debug" >&2; exit 2 ;;
esac
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
# under Firecracker), and the filesystems' images and stores, which a failed
# case leaves, are not results.
find "$results/logs" -type d \( -name rootfs -o -name extents -o -name '*.img' \) -prune \
    -exec rm -rf {} +
find "$results/logs" -type f \( -name '*.img' -o -name rootfs.ext4 \) -delete
echo "Took ${SECONDS}s." | tee -a "$results/host.txt"
cat "$results/comparison.md"
exit "$status"
