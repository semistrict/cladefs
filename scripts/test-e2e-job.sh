#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run cladefs's guest tests (scripts/test-e2e.sh) as a measurement's job on a shared Linux host
# (ubu1), where the results are kept on another filesystem than a host's stores are on: the run
# writes in a directory of its own in the job's temporary one (TMPDIR: fxjob's, on the host's
# scratch disk, short enough for the cases' sockets), so that the cases' stores and the extent
# daemon's part cache, which test-e2e.sh keeps with its results, are on a local disk's ext4, as a
# host's are; and once the run ends, however it does, RESULTS gets what it left, but for the
# stores, the buckets, the part caches and the guests' root filesystems and images, which are no
# results, and the directory goes: the run's logs name it, and this says where the results are.
#
#   scripts/test-e2e-job.sh RESULTS [ARG...]
#
# ARGs are test-e2e.sh's, and so is the environment, but for CLADEFS_E2E_RESULTS, which this sets:
# the bucket is CLADEFS_EXTENTD_REMOTE's, or test-e2e.sh's own, a file bucket in memory, on tmpfs,
# whose remoteness CLADEFS_REMOTE_LATENCY_MS stands for. Its status is test-e2e.sh's, 1 if copying
# the results failed.
#
# A TMPDIR unset, or on another filesystem than ext4, is refused (2) before anything runs, as
# scripts/sandboxes.sh refuses its disk: what a store's syncs cost is its filesystem's. With the
# results on ubu1's home, ZFS, and so the store there, bench-fs-fxfs-guest-dax's sync-loop ran
# 10.8/s at 25 ms of bucket latency, a file bucket there too; 15.9/s with the bucket on tmpfs;
# 30.1/s with the store on the scratch disk besides, one bucket round trip a sync (TASK-397).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
(( $# >= 1 )) || { sed -n '/^#   scripts/p' "$0" >&2; exit 2; }
results=$1
shift
if [[ -z ${TMPDIR:-} ]]; then
    echo "TMPDIR is unset: the run's stores go in the job's temporary directory, fxjob's (run this under fxjob)" >&2
    exit 2
fi
fstype=$(findmnt -no FSTYPE -T "$TMPDIR") ||
    { echo "findmnt doesn't say what filesystem $TMPDIR is on" >&2; exit 2; }
if [[ $fstype != ext4 ]]; then
    echo "$TMPDIR is on $fstype: the run's stores go on ext4, as a host's (TMPDIR, fxjob's on its scratch disk)" >&2
    exit 2
fi
mkdir -p "$results"
results=$(cd "$results" && pwd)
work=$(mktemp -d "$TMPDIR/cladefs-e2e.XXXXXX")
trap 'rm -r "$work"' EXIT
# Reachable by others, as a jailed VM's user must reach it (test-e2e.sh).
chmod 755 "$work"

status=0
CLADEFS_E2E_RESULTS="$work" "$root/scripts/test-e2e.sh" "$@" || status=$?
# What a case leaves of its stores, buckets, caches, images and roots: not results
# (scripts/qualify.sh's, and a jailed VM's jail).
find "$work" -type d \( -name '*.root' -o -name '*.data' -o -name '*.base' -o -name '*-cache' \
    -o -name '.extentd-cache.*' -o -name '*.bucket' -o -name 'store-*' -o -name sandbox-image \
    -o -name rootfs -o -name extents -o -name jail \) -prune -exec rm -r {} +
find "$work" -type f \( -name '*.ext4' -o -name '*.img' \) -delete
cp -R "$work/." "$results/" || status=$(( status ? status : 1 ))
# What the run said of its own directory, as its logs do, is of here now.
echo "Results: $results (the run's $work)" >&2
exit "$status"
