#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Qualify cladefs on the test host, a Linux machine of one's own with KVM, over ssh
# (CLADEFS_TEST_HOST; scripts/lib/host.sh): a worktree there of the commit, which
# origin has, runs the workspace's tests and cladefs's guest tests (guest/tests), in its
# Firecracker, whose DAX window takes KVM's faults through userfaultfd
# (scripts/lib/test-linux-host.sh), under fxjob; the results come back to RESULTS (default: a
# temporary directory), and the worktree goes.
#
# Usage: test-host.sh [RESULTS]
#        test-host.sh resume RUN RESULTS
#        test-host.sh cancel RUN
#
# resume waits for the run RUN, as the test host names it (run.txt's in its results), whose driver
# stopped waiting for it, and fetches its results as a run would; cancel ends it (host_actions).
#
# CLADEFS_E2E_TEST names the guest tests' cases to run, separated by spaces, by name or `--tag
# SUITE`, as scripts/test-e2e.sh's arguments, instead of its default suite, each CLADEFS_E2E_REPEATS
# times, under Firecracker's seccomp filter with CLADEFS_FIRECRACKER_SECCOMP, in its jailer's jail
# with CLADEFS_FIRECRACKER_JAIL (scripts/test-e2e.sh), fxfs-guest-checks' checks alone with
# CLADEFS_CHECKS (such as `sparse,acls`), their budgets scaled by CLADEFS_CHECKS_SCALE (such as 2,
# on a test host others load). CLADEFS_GUEST_KERNEL names cladefs's x86-64 guest kernel here
# (scripts/build-guest-kernel.sh), for the guest Fxfs tests, or CLADEFS_GUEST_KERNEL_BUILD (release,
# debug, kasan, coverage or rust-checks) has the test host build one from the source, or take the
# one built before from the same sources (host_kernel, scripts/lib/linux-host-setup.sh), a release
# one if neither is set; CLADEFS_GUEST_CMDLINE adds to its arguments (such as
# cladefs.follow_apply_delay_us=200). CLADEFS_TEST_HOST_LIMIT caps the run's time on the test host
# (timeout's DURATION, default 6h). CLADEFS_WORKSPACE_TESTS=0 runs the guest tests alone,
# CLADEFS_GUEST_TESTS=0 the workspace's alone, as an arm64 test host runs them (the guest tests boot
# an x86-64 guest kernel: host_need_arch), and CLADEFS_TEST_ARGS are cargo nextest's arguments for
# the workspace's, such as a filter. CLADEFSD_BUCKET, if set, names a bucket the guest tests' extent
# daemons keep their cells in (a bucket's URL the test host reaches), under a prefix
# of the run's own. Every other CLADEFS_*, CLADEFSD_* and PJDFSTEST_* variable set here reaches the
# test host too (scripts/lib/host.sh), such as CLADEFS_FOLLOW_SECONDS and CLADEFS_PJDFSTEST_TESTS,
# and the run's CLADEFS_TEST_HOST_* settings are scripts/lib/host.sh's.
set -euo pipefail
[[ ${CLADEFS_E2E_TEST:-} =~ ^[a-z0-9\ -]*$ ]] || { echo "CLADEFS_E2E_TEST selects test cases" >&2; exit 2; }
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo/scripts/lib/host.sh"
limit=6h

# finish STATUS says how the run went, its last lines, up to four, and returns STATUS.
finish() {
    local lines
    lines=$(wc -l < "$results/remote.log")
    sed -n "$((lines > 4 ? lines - 3 : 1)),\$p" "$results/remote.log" >&2
    echo "Results: $results" >&2
    return "$1"
}
host_after=finish
host_actions "$@"
(( $# <= 1 )) || { echo "Usage: $0 [RESULTS] | resume RUN RESULTS | cancel RUN" >&2; exit 2; }
results=${1:-$(mktemp -d "${TMPDIR:-/tmp}/cladefs-host.XXXXXX")}

status=0
if [[ ${CLADEFS_GUEST_TESTS:-1} != 0 ]]; then
    host_need_arch x86_64 "test-host.sh: the guest tests boot an x86-64 guest kernel" \
        "(CLADEFS_GUEST_TESTS=0 runs the workspace's tests alone)"
fi
host_start
echo "Results: $results" >&2
# The run's own prefix of the bucket, if one is named.
remote=
if [[ -n ${CLADEFSD_BUCKET:-} ]]; then remote=${CLADEFSD_BUCKET%/}/$host_run; fi
# shellcheck disable=SC2086 # The tests' selectors are words separated by spaces.
host_job "CLADEFSD_BUCKET=$remote" test-linux-host.sh ${CLADEFS_E2E_TEST:-} \
    > "$results/remote.log" 2>&1 || status=$?
if [[ $host_state == ended ]]; then
    host_fetch || status=$?
fi
finish "$status"
