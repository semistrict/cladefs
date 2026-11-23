#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Measure cladefs-gc's sweeps over a bucket of files of many cells on the test host, an x86-64 Linux
# machine of one's own, over ssh (CLADEFS_TEST_HOST; scripts/lib/host.sh), as a
# measurement, alone there and on CPUs of its own (hotbed run --bench) (TASK-395.5): scripts/gc-sweep.sh,
# through scripts/lib/gc-sweep-linux-host.sh, fills a bucket of about 1M objects in 10k cells and
# times sweeps of it by passes at once, with their peak memory. The table, the test host and the
# source measured go to docs/measurements/HOST-gc-sweep-<time>-<mark>, the run's mark, unless named,
# with what the run was for (note.md).
#
# Each SIDE=COMMIT, a commit origin has, is a worktree there whose cladefs-gc this tree's scripts
# build and measure beside the others', in one job, the sides' sweeps in turn (scripts/gc-sweep.sh),
# for an A/B (TASK-395.17); each's record in RESULTS/SIDE/source.txt. With none, the run's commit's
# (scripts/lib/host.sh's CLADEFS_TEST_HOST_COMMIT).
#
#   scripts/gc-sweep-host.sh [RESULTS [SIDE=COMMIT...]]
#   scripts/gc-sweep-host.sh resume RUN RESULTS | cancel RUN
#
# CLADEFS_GC_TENANTS, CLADEFS_GC_CELLS, CLADEFS_GC_SEGMENTS, CLADEFS_GC_LARGEST, CLADEFS_GC_FILL,
# CLADEFS_GC_AT_ONCE, CLADEFS_GC_SWEEPS, CLADEFS_GC_ROUNDS and CLADEFS_GC_CELL shape the bucket and
# the sweeps (scripts/gc-sweep.sh), and reach the test host, as every other CLADEFS_* and CLADEFSD_*
# does. CLADEFS_BENCH_NOTE says what the run is for; CLADEFS_TEST_HOST_LIMIT caps it (default 3h).
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo/scripts/lib/host.sh"
limit=3h
host_bench=1
note="cladefs-gc's sweeps over a bucket of files of many cells (TASK-395.5)."

# finish STATUS writes what the run was for, shows the table, and returns STATUS.
finish() {
    {
        echo "${CLADEFS_BENCH_NOTE:-$note}"
        echo
        echo "Host: $host."
    } > "$results/note.md"
    if [[ -f $results/gc-sweep.md ]]; then cat "$results/gc-sweep.md" >&2; fi
    echo "Results: $results" >&2
    return "$1"
}
host_after=finish
host_actions "$@"
usage="Usage: $0 [RESULTS [SIDE=COMMIT...]] | resume RUN RESULTS | cancel RUN"
[[ ${1:-} != *=* ]] || { echo "$usage" >&2; exit 2; }
results=${1:-$repo/docs/measurements/$(host_label)-gc-sweep-$(date -u +%Y-%m-%dT%H%MZ)-$host_mark}
shift $(( $# > 0 ))
pairs=("$@")
trees=()
for pair in ${pairs[@]+"${pairs[@]}"}; do
    [[ $pair == *=* ]] || { echo "$usage" >&2; exit 2; }
    trees+=("${pair%%=*}=$host_dir/trees/${pair%%=*}")
done

status=0
host_start ${pairs[@]+"${pairs[@]}"}
echo "Results: $results" >&2
host_job gc-sweep-linux-host.sh ${trees[@]+"${trees[@]}"} > "$results/remote.log" 2>&1 ||
    status=$?
if [[ $host_state == ended ]]; then
    host_fetch || status=$?
fi
finish "$status"
