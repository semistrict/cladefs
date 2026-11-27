#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Time data-analysis sandboxes from fork to answers (fxfs-guest-python-sandbox, TASK-665) on an
# x86-64 Linux KVM host, as root (scripts/python-sandbox-host.sh drives it): the case's three
# extent daemons keep their cells in RustFS's bucket (scripts/test-e2e-rustfs.sh), its data on the
# work directory's disk; the case's log, what each sandbox took and cost, goes to
# RESULTS/python-sandbox.log, the guest tests' results to RESULTS/e2e.
#
# CLADEFS_TEST_HOST_PHASE (linux-host-setup.sh's host_phase) is what of it runs: `build` builds the
# cases' artifacts and the release guest kernel into the work directory, as work; `measure` runs the
# case against them, building nothing; unset, both.
#
# Usage: python-sandbox-linux-host.sh SOURCE RESULTS
set -euo pipefail
repo=$(cd "${1:?source required}" && pwd)
results=${2:?results directory required}
source "$(dirname "${BASH_SOURCE[0]}")/linux-host-setup.sh"
host_setup
host_tree bench-tree
artifacts=$work/artifacts-python-sandbox
if [[ $host_phase != measure ]]; then
    host_kernel release
    host_artifacts "$artifacts" python-sandbox "$host_kernel_image"
    if [[ $host_phase == build ]]; then
        echo "Built in ${SECONDS}s." | tee "$results/build.txt"
        exit 0
    fi
fi
host_prebuilt "$artifacts"
{
    host_record
    findmnt -no SOURCE,FSTYPE,OPTIONS -T "$work"
    podman --version
} > "$results/host.txt"

bucket=$work/python-sandbox-bucket
if [[ -e $bucket ]]; then rm -r -- "$bucket"; fi
mkdir -p "$bucket"
status=0
CLADEFS_E2E_RESULTS="$results/e2e" CLADEFS_RUSTFS_DATA="$bucket" \
    CLADEFS_RUSTFS_LOG="$results/rustfs.log" \
    "$repo/scripts/test-e2e-rustfs.sh" fxfs-guest-python-sandbox > "$results/e2e.log" 2>&1 ||
    status=$?
find "$results/e2e" -name log.txt -path '*fxfs-guest-python-sandbox*' \
    -exec grep -h '^pysandbox: ' {} + | tee "$results/python-sandbox.log" || true
rm -r -- "$bucket"
# What the case leaves of its cells, caches, image and root when it fails: not results.
find "$results/e2e" -depth -type d \( -name '*.root' -o -name '*.base' -o -name '*-cache' \
    -o -name sandbox-image -o -name rootfs -o -name '*-disk[0-9]*' \) -exec rm -r -- {} +
exit "$status"
