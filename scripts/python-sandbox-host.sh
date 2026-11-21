#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Time data-analysis sandboxes from fork to answers on the test host, an x86-64 Linux machine of
# one's own, over ssh (CLADEFS_TEST_HOST; scripts/lib/host.sh), as a measurement,
# alone there and on CPUs of its own (fxjob --bench) (TASK-665): guest/tests' case
# fxfs-guest-python-sandbox, three extent daemons with part caches and each other as peers, one
# RustFS bucket 25 ms away, a base of uv-installed pandas and marimo with no bytecode, and its
# sandboxes, cold then warm, each timed by phase (scripts/lib/python-sandbox-linux-host.sh). The
# results go to docs/measurements/HOST-python-sandbox-<time>-<mark>, the run's mark, unless named,
# with what the run was for (note.md).
#
#   scripts/python-sandbox-host.sh [RESULTS]
#   scripts/python-sandbox-host.sh resume RUN RESULTS | cancel RUN
#
# CLADEFS_PYSANDBOX_ROUNDS is how many sandboxes each host runs (default 2: cold, then warm).
# CLADEFS_PYSANDBOX_WORKLOADS lists, comma-separated, what the sandboxes run in turn: `notebook`
# (the default), `query` (TASK-702.1). Each sandbox's trace of its reads is kept in the results,
# under e2e/, as traces/NN-SANDBOX-WORKLOAD.reads, for scripts/read_prediction.py.
# CLADEFS_PYSANDBOX_COLD, set, adds a cold host's first boot after the rounds, its version's read
# evidence fetched ahead, with cold peers (every host restarted) and with warm ones (TASK-702.3).
# CLADEFS_BENCH_NOTE says what the run is for. Every other CLADEFS_*, CLADEFSD_* and PJDFSTEST_*
# variable set here reaches the test host too (scripts/lib/host.sh).
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo/scripts/lib/host.sh"
limit=2h
host_bench=1

# finish STATUS writes what the run was for, says each sandbox's phases, and returns STATUS.
finish() {
    {
        echo "${CLADEFS_BENCH_NOTE:-Data-analysis sandboxes timed from fork to answers (TASK-665).}"
        echo
        echo "Host: $host."
    } > "$results/note.md"
    grep -h '^pysandbox: ' "$results/remote.log" >&2 || true
    echo "Results: $results" >&2
    return "$1"
}
host_after=finish
host_actions "$@"
(( $# <= 1 )) || { echo "Usage: $0 [RESULTS] | resume RUN RESULTS | cancel RUN" >&2; exit 2; }
results=${1:-$repo/docs/measurements/$(host_label)-python-sandbox-$(date -u +%Y-%m-%dT%H%MZ)-$host_mark}

status=0
host_need_arch x86_64 "python-sandbox-host.sh: its sandboxes boot an x86-64 guest kernel"
host_start
echo "Results: $results" >&2
host_job python-sandbox-linux-host.sh > "$results/remote.log" 2>&1 || status=$?
if [[ $host_state == ended ]]; then
    host_fetch || status=$?
fi
finish "$status"
