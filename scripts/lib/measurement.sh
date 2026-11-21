# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# No build inside a measurement (TASK-368). A measurement, a job of `fxjob --bench`'s, holds cores
# 4-7, most of the L3, memory bandwidth and up to 16 GiB away from every other measurement on the
# host for as long as it runs: a build inside it held them as long as it compiled, an hour on 4
# cores for two trees, and kept every other measurement waiting. So a measurement's builds run
# first, as ordinary work (fxjob without --bench: scripts/lib/host.sh's host_job does, and
# scripts/bench-rounds.sh), into artifacts the measurement then runs (scripts/test-e2e.sh's
# CLADEFS_E2E_BUILD_ONLY, then CLADEFS_E2E_PREBUILT); and what would build refuses to inside one,
# saying so, so that a driver that forgets fails at once rather than hold the bench quietly.
# Called where a build would start: scripts/test-e2e.sh's, scripts/test.sh, and
# scripts/lib/linux-host-setup.sh's host_kernel and host_cargo, scripts/gc-sweep.sh's cargo.
#
# A measurement is told by its cgroup, not by anything its caller says: fxjob runs it in a scope
# of fxdax-bench.slice (the test host's setup), where `sudo` inside it stays,
# and which no variable a script forgot to pass can lose. CLADEFS_SELF_CGROUP names the file that
# says this process's cgroup (default /proc/self/cgroup), for the tests. Sourcing this defines its
# functions and does nothing else.

# measuring succeeds if this process runs in a measurement: in fxjob --bench's slice.
measuring() {
    local cgroup=${CLADEFS_SELF_CGROUP:-/proc/self/cgroup}
    [[ -r $cgroup && $(< "$cgroup") == */fxdax-bench.slice/* ]]
}

# measurement_refuses WHAT exits 2, saying why, if this process runs in a measurement, which WHAT,
# a build, would hold for as long as it builds.
measurement_refuses() {
    measuring || return 0
    echo "$1 would build inside a measurement (fxjob --bench, $(< "${CLADEFS_SELF_CGROUP:-/proc/self/cgroup}")), holding its cores and memory from every other measurement for as long as it builds: build first as work (fxjob without --bench; scripts/lib/host.sh's host_job, test-e2e.sh's CLADEFS_E2E_BUILD_ONLY), then measure what it built (CLADEFS_E2E_PREBUILT) (TASK-368)" >&2
    exit 2
}
