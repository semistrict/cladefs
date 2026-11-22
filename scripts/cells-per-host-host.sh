#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# How many cells a host holds (board 5586): the cells-per-host cases, without DAX and with it, on
# the test host, one run of scripts/test-host.sh as a measurement there, alone and on CPUs of its
# own (CLADEFS_TEST_HOST_BENCH=1), in a job of CLADEFS_TEST_HOST_MEM (default 16G, what the
# recorded run and cells-per-host.py's floors had), a release guest kernel unless
# CLADEFS_GUEST_KERNEL_BUILD says otherwise, each Firecracker in its jailer's jail as production
# runs it unless CLADEFS_FIRECRACKER_JAIL is set empty. Each case adds VMs on forks of one base
# until the host holds no more (guest/tests/test_cases/src/test_cells_per_host.rs); CLADEFS_CELLS_*
# set how, and reach the test host, as every CLADEFS_* and CLADEFSD_* does (test-host.sh). Then
# scripts/cells-per-host.py over the run: RESULTS/cells.md and RESULTS/cells.json, each case's
# VMs held, the limit that stopped it and its steps. It fails if the run does, or a case held
# fewer VMs than its floor (cells-per-host.py's, from the recorded measurement).
#
# Usage: cells-per-host-host.sh RESULTS
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
(( $# == 1 )) || { echo "Usage: $0 RESULTS" >&2; exit 2; }
results=$1
export CLADEFS_WORKSPACE_TESTS=0 CLADEFS_E2E_REPEATS=1 CLADEFS_TEST_HOST_BENCH=1
export CLADEFS_TEST_HOST_MEM=${CLADEFS_TEST_HOST_MEM:-16G}
export CLADEFS_GUEST_KERNEL_BUILD=${CLADEFS_GUEST_KERNEL_BUILD:-release}
export CLADEFS_FIRECRACKER_JAIL=${CLADEFS_FIRECRACKER_JAIL-1}
status=0
CLADEFS_E2E_TEST="--tag cells-per-host" "$repo/scripts/test-host.sh" "$results/run" || status=$?
"$repo/scripts/cells-per-host.py" --json "$results/cells.json" "$results/run" \
    > "$results/cells.md" || status=$?
cat "$results/cells.md" >&2
exit "$status"
