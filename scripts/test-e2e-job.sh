#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run cladefs's guest tests (scripts/test-e2e.sh) as a measurement's round in a hotbed workspace
# on a test host: the run writes in a directory of its own in the command's temporary one (TMPDIR,
# the workspace's, short enough for the cases' sockets), so that the cases' cells and the extent
# daemon's part cache, which test-e2e.sh keeps with its results, are on one disk of the round's
# own; and once the run ends, however it does, RESULTS gets what it left, but for the cells, the
# buckets, the part caches and the guests' root filesystems and images, which are no results, and
# the directory goes: the run's logs name it, and this says where the results are. The run is a
# round under scripts/contamination.py (TASK-375), which records what others wrote meanwhile to the
# disk that directory is on: its record, contamination.json, is in CLADEFS_BENCH_RESULTS, beside the
# bench's report, where scripts/bench-ab.py reads both, or in RESULTS if the run has none. Copying
# the results back is after the round, and none of it.
#
#   scripts/test-e2e-job.sh RESULTS [ARG...]
#
# ARGs are test-e2e.sh's, and so is the environment, but for CLADEFS_E2E_RESULTS, which this sets:
# the bucket is CLADEFSD_BUCKET's, or test-e2e.sh's own, a file bucket in memory, on tmpfs,
# whose distance CLADEFS_BUCKET_LATENCY_MS stands for. Its status is test-e2e.sh's, 1 if copying
# the results failed. A cell's syncs cost what its filesystem's do: the workspace's, ZFS on the
# fleet's hosts, since the measurements left the ext4 scratch disk (board 6058).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
(( $# >= 1 )) || { sed -n '/^#   scripts/p' "$0" >&2; exit 2; }
results=$1
shift
work_root=${TMPDIR:-/tmp}
mkdir -p "$results"
results=$(cd "$results" && pwd)
work=$(mktemp -d "$work_root/cladefs-e2e.XXXXXX")
trap 'rm -r "$work"' EXIT
# Reachable by others, as a jailed VM's user must reach it (test-e2e.sh).
chmod 755 "$work"

status=0
CLADEFS_E2E_RESULTS="$work" "$root/scripts/contamination.py" run \
    "${CLADEFS_BENCH_RESULTS:-$results}" "$work" -- "$root/scripts/test-e2e.sh" "$@" || status=$?
# What a case leaves of its cells, buckets, caches, images and roots: not results
# (scripts/qualify.sh's, and a jailed VM's jail).
find "$work" -type d \( -name '*.root' -o -name '*.data' -o -name '*.base' -o -name '*-cache' \
    -o -name '.cladefsd-cache.*' -o -name '*.bucket' -o -name 'store-*' -o -name sandbox-image \
    -o -name rootfs -o -name extents -o -name jail -o -name '*-disk[0-9]*' -o -name '.cladefsd-disk*' \) \
    -prune -exec rm -r {} +
find "$work" -type f \( -name '*.ext4' -o -name '*.img' \) -delete
cp -R "$work/." "$results/" || status=$(( status ? status : 1 ))
# What the run said of its own directory, as its logs do, is of here now.
echo "Results: $results (the run's $work)" >&2
exit "$status"
