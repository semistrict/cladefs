#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Qualify cladefs on a Linux KVM host, as root (its guest tests on an x86-64 one). It runs the
# workspace's tests (unless CLADEFS_WORKSPACE_TESTS=0; CLADEFS_TEST_ARGS, split at
# spaces, are cargo nextest's arguments, such as a filter), then cladefs's guest
# tests (unless CLADEFS_GUEST_TESTS=0; scripts/test-e2e.sh; TESTs name them),
# whose DAX window takes KVM's faults through userfaultfd. With CLADEFS_TEST_HOST_TRACE, a bpftrace
# script of SOURCE's traces the host as the guest tests run, its maps in RESULTS/host-trace.txt
# (scripts/extent-transport.bt, TASK-164.11.1). With CLADEFS_GUEST_KERNEL_BUILD set
# (release; debug: lockdep, KCSAN, DEBUG_ATOMIC_SLEEP; kasan: KASAN, KFENCE, lockdep; or coverage:
# gcov of fs/cladefs, which leaves coverage/ in the results, scripts/guest-coverage.py), the guest
# tests run on a guest kernel it builds from the source (scripts/build-guest-kernel.sh), not
# CLADEFS_GUEST_KERNEL's; with neither set, on a release one it builds (TASK-130: without one, every
# guest case skipped). It builds in trees that persist between runs, so that a run builds only
# what changed.
#
# As a measurement's run (CLADEFS_TEST_HOST_BENCH=1, scripts/perf-gate-host.sh's), it runs in two
# jobs (linux-host-setup.sh's host_phase): `build`, as work, the workspace's tests, the guest kernel
# and the guest tests' artifacts (test-e2e.sh's CLADEFS_E2E_BUILD_ONLY); then `measure`, alone, the
# guest tests against those, building nothing, nor running the harness's own tests (TASK-368). A
# coverage kernel's counts are read against its build, so that runs in one job alone.
#
# Usage: test-linux-host.sh SOURCE RESULTS [TEST...]
set -euo pipefail
repo=$(cd "${1:?source required}" && pwd)
results=${2:?results directory required}
shift 2
source "$(dirname "${BASH_SOURCE[0]}")/linux-host-setup.sh"
host_setup
host_tree test-tree
host_need cargo-nextest

status=0
host_record > "$results/host.txt"

# scripts/test.sh's parts but the Go client's, which CI's go job runs: these hosts have no Go
# (go.mod wants 1.26, newer than Ubuntu's), and the part, added by TASK-219, failed every run here
# with exit 127, the parts after it unrun (2026-11-07, TASK-222's run).
export CLADEFS_TEST_PARTS=${CLADEFS_TEST_PARTS:-clippy workspace sim shuttle scripts guest firecracker}

if [[ $host_phase != measure && ${CLADEFS_WORKSPACE_TESTS:-1} != 0 ]]; then
    echo "== cladefs tests" >&2
    read -ra test_args <<< "${CLADEFS_TEST_ARGS:-}"
    "$repo/scripts/test.sh" --no-fail-fast "${test_args[@]}" \
        > "$results/cladefs-tests.log" 2>&1 || status=$?
    echo "cladefs tests: exit $status" >&2
fi

if [[ ${CLADEFS_GUEST_TESTS:-1} == 0 ]]; then
    exit "$status"
fi
artifacts=$work/artifacts-e2e
if [[ $host_phase != measure ]]; then
    if [[ -z ${CLADEFS_GUEST_KERNEL:-} ]]; then
        CLADEFS_GUEST_KERNEL_BUILD=${CLADEFS_GUEST_KERNEL_BUILD:-release}
    fi
    case ${CLADEFS_GUEST_KERNEL_BUILD:-} in
        '') ;;
        release | debug | kasan | coverage | rust-checks)
            if [[ $CLADEFS_GUEST_KERNEL_BUILD == coverage && $host_phase != all ]]; then
                echo "A coverage kernel's counts are read against its build: no measurement's" >&2
                exit 2
            fi
            echo "== guest kernel ($CLADEFS_GUEST_KERNEL_BUILD)" >&2
            if ! host_kernel "$CLADEFS_GUEST_KERNEL_BUILD"; then
                echo "guest kernel: failed, see guest-kernel.log" >&2
                exit 1
            fi
            export CLADEFS_GUEST_KERNEL=$host_kernel_image
            ;;
        *)
            echo "CLADEFS_GUEST_KERNEL_BUILD is release, debug, kasan, coverage or rust-checks" >&2
            exit 2
            ;;
    esac
    if [[ $host_phase == build ]]; then
        echo "== guest tests' artifacts" >&2
        host_artifacts "$artifacts" e2e "${CLADEFS_GUEST_KERNEL:-}" || exit 1
        exit "$status"
    fi
else
    host_prebuilt "$artifacts"
fi
if [[ -n ${CLADEFS_GUEST_KERNEL_BUILD:-} ]]; then
    # Its kind, which the guest tests' time limits scale by (TASK-168).
    export CLADEFS_GUEST_KERNEL_BUILD
fi
if [[ -n ${CLADEFS_GUEST_KERNEL:-} ]]; then
    sha256sum "$CLADEFS_GUEST_KERNEL" > "$results/guest-kernel.txt"
fi
if [[ ${CLADEFS_GUEST_KERNEL_BUILD:-} == coverage ]]; then
    export CLADEFS_COVERAGE="$results/coverage-counts"
    mkdir -p "$CLADEFS_COVERAGE"
fi
# A crash leaves a core, whose threads' stacks the results keep, and only those, as the host goes
# with the core: a VMM died of SIGSEGV in a soak, and nothing said where (2026-10-31). Private
# memory alone (coredump_filter): a guest's and a daemon's arenas are shared. Not on a test host
# (CLADEFS_TEST_HOST_PROVISIONED), whose core pattern is that of every process there,
# others' runs' too, and its own setup's.
cores="$results/cores"
mkdir -p "$cores"
if [[ ${CLADEFS_TEST_HOST_PROVISIONED:-} != 1 ]]; then
    sysctl -q -w kernel.core_pattern="$cores/core.%p"
    ulimit -c unlimited
    echo 0x11 > /proc/self/coredump_filter
fi
echo "== guest tests" >&2
tracer=
if [[ -n ${CLADEFS_TEST_HOST_TRACE:-} ]]; then
    host_need bpftrace
    # What it says, its first MiB, the rest read and dropped, which a closed pipe would kill it
    # for: extent-transport.bt had it say 700 MB over a 30-minute case (GCE r810, r811).
    bpftrace -q -o "$results/host-trace.txt" "$repo/$CLADEFS_TEST_HOST_TRACE" \
        2> >({ head -c 1048576; cat > /dev/null; } > "$results/host-trace.log") &
    tracer=$!
fi
e2e_status=0
# A jailed run's jailer (its user, chroot and devices) needs root, which a hotbed workspace's user
# has through sudo (AGENTS.md): as root, as CI runs it, and its results given back after.
as_root=()
if [[ -n ${CLADEFS_FIRECRACKER_JAIL:-} ]] && (( EUID != 0 )); then
    as_root=(sudo --preserve-env env "PATH=$PATH" "HOME=$HOME")
fi
CLADEFS_E2E_RESULTS="$results/e2e" ${as_root[@]+"${as_root[@]}"} "$repo/scripts/test-e2e.sh" "$@" \
    > "$results/e2e.log" 2>&1 || e2e_status=$?
if (( ${#as_root[@]} )); then
    sudo chown -R "$(id -u):$(id -g)" "$results"
fi
echo "guest tests: exit $e2e_status" >&2
# The tracer prints its maps as it is interrupted.
if [[ -n $tracer ]]; then
    kill -INT "$tracer"
    wait "$tracer" || echo "host trace: exit $? ($results/host-trace.log)" >&2
fi
for core in "$cores"/core.*; do
    [[ -f $core ]] || continue
    echo "guest tests: a process crashed: $core.stacks.txt" >&2
    eu-stack -l -i -n 512 --core="$core" > "$core.stacks.txt" 2>&1 || true
    rm "$core"
done
if [[ -n ${CLADEFS_COVERAGE:-} ]]; then
    "$repo/scripts/guest-coverage.py" "$CLADEFS_COVERAGE" "$(dirname "$(find "$host_kernel_work" \
        -path '*/fs/cladefs/dax.gcno' -print -quit)")" "$results/coverage" ||
        echo "guest coverage: failed" >&2
fi
if (( status == 0 )); then status=$e2e_status; fi
exit "$status"
