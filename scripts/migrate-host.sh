#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Move a VM between hosts through a bucket (TASK-28.11), in hotbed workspaces on test hosts
# (scripts/lib/host.sh). On the first, the guest test fxfs-guest-migrate-dax runs a VM
# whose cell its extent daemon keeps in the bucket, and snapshots it, the snapshot, its
# memory and the VM's root filesystem left in a bundle; then the run there ends, its directory gone;
# on the second, which has nothing of the cell, the same test restores the VM from the bundle, its
# cell forked from the snapshot's version, whose cell the daemon takes from the bucket, and the
# guest checks what it wrote before, and writes and reads more (test_fxfs_guest_snapshot.rs). Both
# run the commit from the same paths, as the restored VM goes on reading its root filesystem where
# it was. The test hosts' results go to RESULTS/a and RESULTS/b; the bundle, and the bucket if it is
# the run's own, go as the run ends, whatever happens.
#
#   scripts/migrate-host.sh [RESULTS]
#
# Both runs are on one node, CLADEFS_TEST_HOST's or the one hotbed picks, the second after the
# first, in the session's one workspace there: hotbed carries no files between two nodes'
# workspaces yet, which a bundle would need (board 7817). CLADEFS_MIGRATE_BUCKET names the bucket
# and prefix the run keeps its cells under, under a prefix of the run's own, which the run leaves;
# by default, a directory of the run's own in the workspace (file://). CLADEFS_GUEST_KERNEL names a
# guest kernel in the workspace, or CLADEFS_GUEST_KERNEL_BUILD the one it builds (release by
# default, scripts/test-host.sh). Every other CLADEFS_*, CLADEFSD_* and PJDFSTEST_* variable set
# here reaches the workspace too (scripts/lib/host.sh).
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo/scripts/lib/host.sh"
limit=2h
if [[ -n ${CLADEFS_MIGRATE_HOSTS:-} ]]; then
    echo "CLADEFS_MIGRATE_HOSTS is gone: both runs are on one node, CLADEFS_TEST_HOST's (board 7817)" >&2
    exit 2
fi
top=${1:-$(mktemp -d "${TMPDIR:-/tmp}/cladefs-migrate.XXXXXX")}
mkdir -p "$top"
top=$(cd "$top" && pwd)
# What outlasts each host's run: the bundle, and the bucket if it is the run's own.
held=$host_dir.held
# Both runs in the one workspace, which holds the bundle between them.
host_keep_session=1
if [[ -n ${CLADEFS_MIGRATE_BUCKET:-} ]]; then
    [[ $CLADEFS_MIGRATE_BUCKET =~ ^[a-z0-9]+://[A-Za-z0-9._/-]*$ ]] ||
        { echo "CLADEFS_MIGRATE_BUCKET is a bucket's URL, as s3://bucket/prefix" >&2; exit 2; }
    remote=${CLADEFS_MIGRATE_BUCKET%/}/$host_run
else
    remote=file://$held/bucket
fi

# cleanup, the trap, ends the run, and removes what it held, once a run placed it.
cleanup() {
    local status=$?
    trap - EXIT
    host_end || status=1
    if [[ -n $host_node || $host == local ]]; then
        host_shell "rm -rf -- $(printf %q "$held")" > /dev/null || status=1
    fi
    host_session_end || status=1
    echo "Results: $top" >&2
    exit "$status"
}
trap cleanup EXIT

# migrate SIDE PHASE TOKEN runs the test's PHASE, alone, as SIDE (0 for a, 1 for b), as of the
# placement TOKEN, its cells in the bucket, its results in RESULTS/a or b; and ends the run.
migrate() {
    local side=$1 phase=$2 token=$3 status=0
    results=$top/$(tr 01 ab <<< "$side")
    # host_start's trap is this script's.
    host_need_arch x86_64 "migrate-host.sh: its VM boots an x86-64 guest kernel"
    host_start || status=$?
    trap cleanup EXIT
    (( status == 0 )) || return "$status"
    host_shell "mkdir -p $(printf %q "$held")" || return
    host_job CLADEFS_WORKSPACE_TESTS=0 "CLADEFSD_BUCKET=$remote/cells" \
        "CLADEFSD_TOKEN=$token" "CLADEFS_MIGRATE_PHASE=$phase" "CLADEFS_MIGRATE_BUNDLE=$held/bundle" \
        test-linux-host.sh fxfs-guest-migrate-dax > "$results/remote.log" 2>&1 || status=$?
    if [[ $host_state == ended ]]; then
        host_fetch || status=$?
    fi
    sed -n '$p' "$results/remote.log" >&2
    host_end || status=1
    return "$status"
}

migrate 0 source 1
# The second run's own directory, of a name of its own, on the first's node.
host_name "$host_run-b"
# A later placement than the first run's.
migrate 1 target 2
echo "The VM moved: restored on $(host_label) from what the first run left in $remote." >&2
