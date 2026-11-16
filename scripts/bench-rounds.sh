#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Rounds of guest test cases on this Linux host, a shared one (ubu1), each case run once a round,
# the cases' order reversed every other round (A B, B A, ...), so that what drifts on the host
# over the hours moves every case alike (TASK-350). Each run is a scripts/test-e2e.sh of the one
# case, as root (its cases attach loop devices), as a measurement: `fxjob --bench`, which gives
# it cores, most of the L3, memory bandwidth and memory of its own whatever else the host runs,
# or fails the run (ubu1's system-setup.d/146-fxdax_cgroups.sh). So a run doesn't wait for the
# host to be quiet: what the rest of the host does reaches it only through what isolation can't
# split, the disks (ZFS schedules its own IO) and the kernel's shared locks. Before each run it
# drops the host's caches, the page cache and what of ZFS's ARC can go; and it records, in
# RESULTS/runs.tsv, each run's round, case, outcome, the host's load and the ARC's size before
# and after, and the time the run's tasks stalled (pressure stall information of
# fxdax-bench.slice, in ms, `some`) waiting on CPU, memory and IO: what a run's numbers are
# read against.
#
#   scripts/bench-rounds.sh RESULTS ROUNDS CASE...
#
# The cases' artifacts are built once, first, into RESULTS/artifacts (CLADEFS_E2E_BUILD_ONLY), as
# ordinary work (fxjob without --bench, at niceness 19, CARGO_BUILD_JOBS at once, 4 unless the
# caller says, into the caller's CARGO_TARGET_DIR if one is set), and every run boots those
# (CLADEFS_E2E_PREBUILT): no run builds, which a measurement refuses to (scripts/lib/measurement.sh,
# TASK-368), nor runs the harness's own tests.
# Each run may hold CLADEFS_ROUNDS_MEM (16G) of memory, page cache, the extent daemon's and its
# bucket together.
#
# A run's numbers are cladefs's on a host's disks, not this host's results' filesystem's (ubu1's
# home, ZFS, whose syncs a durable sync then measured: bench-fs-fxfs-guest-dax's sync-loop 10.8/s
# there at 25 ms of bucket latency, 30.1/s as below, TASK-397). Each run's stores and its extent
# daemon's part cache are on ext4, as a host's local disk: in the job's temporary directory,
# fxjob's, on the host's scratch disk, which goes with the job (scripts/test-e2e-job.sh, which
# refuses another filesystem). Its bucket is test-e2e.sh's own, a file bucket in memory, on
# tmpfs, whose distance CLADEFS_BUCKET_LATENCY_MS stands for, unless CLADEFSD_BUCKET names
# one: it counts against the run's memory, and a case whose working set of gigabytes overruns
# that (TASK-350) takes more of it (CLADEFS_ROUNDS_MEM), or a bucket named on a disk.
#
# Each run's results go to RESULTS/CASE/round-K: test-e2e.sh's logs (e2e, copied there as the run
# ends, without the stores and the guests' root filesystems), and the fs bench's report as JSON
# (CLADEFS_BENCH_RESULTS), which scripts/bench-ab.py compares, a side's directories its cases'
# round directories; and what others wrote to the run's disk meanwhile (contamination.json there,
# scripts/contamination.py, TASK-375), the job's scratch disk, the stores', which only the job
# can name: test-e2e-job.sh runs the cases under it, a run others wrote beside left out of the
# comparison's timings. The environment's CLADEFS_* and CLADEFSD_* variables pass to every run (sudo drops them
# otherwise): CLADEFS_GUEST_KERNEL, CLADEFS_BENCH_ITERATIONS, CLADEFS_BENCH_GROUPS, CLADEFS_PGBENCH_RUNS
# and such. It fails if a run did; a run refused before it started (2: no ext4 for its stores,
# artifacts it can't boot) ends the rounds there, as every other would be. CLADEFS_TEST_HOST_PROC
# and CLADEFS_TEST_HOST_SYS name the host's /proc and /sys, for the tests.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
(( $# >= 3 )) || { sed -n '/^#   scripts/p' "$0" >&2; exit 2; }
results=$1 rounds=$2
shift 2
cases=("$@")
[[ $rounds =~ ^[1-9][0-9]*$ ]] || { echo "ROUNDS is a count" >&2; exit 2; }
mkdir -p "$results"
results=$(cd "$results" && pwd)
command -v fxjob > /dev/null || { echo "fxjob not found: this runs on a host with fxjob (ubu1)" >&2; exit 2; }

proc=${CLADEFS_TEST_HOST_PROC:-/proc} sys=${CLADEFS_TEST_HOST_SYS:-/sys}
load() { cut -d' ' -f1 "$proc/loadavg"; }
arc_bytes() {
    awk '$1 == "size" { print $3 }' "$proc/spl/kstat/zfs/arcstats" 2> /dev/null || echo -
}

# The time, in µs, fxdax-bench.slice's tasks have stalled on RESOURCE (cpu, memory, io), some
# of them at least.
bench=$sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/fxdax.slice/fxdax-bench.slice
stalled() {
    sed -n 's/^some .*total=//p' "$bench/$1.pressure"
}

# The CLADEFS_* and CLADEFSD_* variables of this environment, as env's NAME=VALUE words.
passed=()
while IFS= read -r name; do
    passed+=("$name=${!name}")
done < <(compgen -e | grep -E '^CLADEFSD?_' || true)

# as_root COMMAND...: COMMAND as root, inside a job, with the job's TMPDIR, fxjob's, which it
# removes with the job: sudo, set-user-ID, never passes it on (glibc drops it), so env sets it,
# expanded by the job's shell.
# shellcheck disable=SC2016 # Expanded by the job's shell.
as_root=(bash -c 'exec sudo env TMPDIR="${TMPDIR:-/tmp}" "$@"' as-root)

# Built as root, as the runs boot it, then given back: with the caller's bounds on cargo, which sudo
# would drop (TASK-446), as scripts/lib/host.sh's runs build, 4 jobs at once by default.
artifacts=$results/artifacts
cargo_env=("CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}")
built=("$repo" "$HOME/.cargo" "$HOME/.rustup" "$results")
if [[ -n ${CARGO_TARGET_DIR:-} ]]; then
    cargo_env+=("CARGO_TARGET_DIR=$CARGO_TARGET_DIR")
    built+=("$CARGO_TARGET_DIR")
fi
fxjob --name rounds-build --mem 8G -- nice -n 19 \
    "${as_root[@]}" "PATH=$PATH" "HOME=$HOME" "${cargo_env[@]}" ${passed[@]+"${passed[@]}"} \
    "CLADEFS_E2E_BUILD_ONLY=$artifacts" "$repo/scripts/test-e2e.sh" "${cases[@]}" \
    > "$results/build.log" 2>&1 || { echo "building failed: $results/build.log" >&2; exit 1; }
sudo chown -R "$(id -u):$(id -g)" "${built[@]}"

[[ -f $results/runs.tsv ]] ||
    printf 'round\tcase\toutcome\tload_before\tload_after\tarc_before\tarc_after\tstarted\t%s\n' \
        'cpu_stall_ms	memory_stall_ms	io_stall_ms' \
        > "$results/runs.tsv"
status=0
for ((round = 1; round <= rounds; round++)); do
    order=("${cases[@]}")
    if (( round % 2 == 0 )); then
        order=()
        for ((i = ${#cases[@]} - 1; i >= 0; i--)); do order+=("${cases[i]}"); done
    fi
    for case in "${order[@]}"; do
        out=$results/$case/round-$round
        mkdir -p "$out"
        sync
        # shellcheck disable=SC2016 # For sh to expand.
        sudo sh -c 'echo 3 > "$1"' drop "$proc/sys/vm/drop_caches"
        load_before=$(load) arc_before=$(arc_bytes) started=$(date -u +%FT%TZ)
        stalls_before=()
        for resource in cpu memory io; do stalls_before+=("$(stalled "$resource")"); done
        outcome=ok ran=0
        fxjob --bench --mem "${CLADEFS_ROUNDS_MEM:-16G}" --name "rounds-$case-$round" -- \
            "${as_root[@]}" "PATH=$PATH" "HOME=$HOME" ${passed[@]+"${passed[@]}"} \
            "CLADEFS_E2E_PREBUILT=$artifacts" "CLADEFS_BENCH_RESULTS=$out" \
            "$repo/scripts/test-e2e-job.sh" "$out/e2e" "$case" > "$out/run.log" 2>&1 || ran=$?
        if (( ran == 2 )); then
            sudo chown -R "$(id -u):$(id -g)" "$results"
            echo "round $round $case: refused, and so would every run be: $out/run.log" >&2
            exit 2
        fi
        if (( ran )); then outcome=failed; fi
        load_after=$(load)
        stalls=()
        i=0
        for resource in cpu memory io; do
            stalls+=($(( ($(stalled "$resource") - stalls_before[i]) / 1000 )))
            i=$((i + 1))
        done
        [[ $outcome == ok ]] || status=1
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$round" "$case" "$outcome" \
            "$load_before" "$load_after" "$arc_before" "$(arc_bytes)" "$started" "${stalls[@]}" \
            >> "$results/runs.tsv"
        echo "round $round $case: $outcome (stalled ms: cpu ${stalls[0]}, memory ${stalls[1]}, io ${stalls[2]})" >&2
    done
done
sudo chown -R "$(id -u):$(id -g)" "$results"
exit "$status"
