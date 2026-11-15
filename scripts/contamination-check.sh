#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Checks, on a host with fxjob (ubu1), that a measurement's round says when others wrote beside it
# (scripts/contamination.py, TASK-375): two rounds, each a measurement of its own (fxjob --bench),
# one alone, one beside a disk loader, then scripts/bench-ab.py's comparison of the two. It takes
# about four minutes once the measurements' slot is free.
#
#   scripts/contamination-check.sh RESULTS
#
# RESULTS is a new directory on the measurements' scratch disk (/mnt/fxdax-scratch on ubu1). A
# round here is no guest's: for CLADEFS_CONTAMINATION_CHECK_SECS seconds (default 90) it rewrites a
# 64 MiB file with fsync in its job's TMPDIR, on the scratch disk, and records each pass's time
# (`pass`, s) and what it wrote (`written`, MiB), as a case does (RESULTS/NAME/bench-check.json).
# The loader rewrites a 64 MiB file with fsync twice a second, 128 MB/s at most, in
# CLADEFS_CONTAMINATION_CHECK_LOAD (default the home, on ubu1 a ZFS dataset of the pool the scratch
# zvol is on), inside the round's own job, started before the round and killed after it: never as
# ordinary work beside a measurement. Its writes are others' to the round by ZFS's count of the
# home's dataset. A loader on the scratch disk itself, inside the round's job, would be the
# round's own cgroup's I/O, and mark nothing: what a loader in another cgroup does to the scratch
# disk's own count this check doesn't show (scripts/tests/test_contamination.py does, on a host
# of its own making).
#
# What it checks: the round alone is clean, its record naming the cgroup whose I/O was taken for
# its own; the round beside the loader is contaminated, saying how much others wrote; and the
# comparison leaves the loaded round's `pass` out, NO RESULT, keeps its `written`, and fails.
# It prints each record's line and the comparison, and exits 1 if a check failed.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
record=$repo/scripts/contamination.py
seed_mib=64

# work OUT SECS: the round's workload, in its job; its measurements to OUT/bench-check.json.
work() {
    local out=$1 secs=$2 seed=$TMPDIR/contamination-check-seed passes=() started
    [[ -n ${EPOCHREALTIME:-} ]] || { echo "bash 5 times the passes (EPOCHREALTIME)" >&2; return 2; }
    dd if=/dev/urandom of="$seed" bs=1M count="$seed_mib" status=none
    SECONDS=0
    while (( SECONDS < secs )); do
        started=${EPOCHREALTIME/,/.}
        dd if="$seed" of="$TMPDIR/contamination-check-work" bs=1M conv=fsync status=none
        passes+=("$(awk -v a="$started" -v b="${EPOCHREALTIME/,/.}" 'BEGIN { printf "%.3f", b - a }')")
    done
    local values
    values=$(IFS=,; echo "${passes[*]}")
    printf '{"case": "check", "backend": "check", "results": [
 {"workload": "pass", "unit": "s", "higher_is_better": false, "values": [%s]},
 {"workload": "written", "unit": "MiB", "higher_is_better": false, "values": [%d]}]}\n' \
        "$values" $(( ${#passes[@]} * seed_mib )) > "$out/bench-check.json"
}

# round OUT SECS [LOAD]: a round, in its job: the workload under the record, beside a loader
# writing in LOAD if one is named, started before it and killed after.
round() {
    local out=$1 secs=$2 load=${3:-} loader=
    mkdir -p "$out"
    if [[ -n $load ]]; then
        local seed=$TMPDIR/contamination-check-load file=$load/contamination-check-loader.$$
        dd if=/dev/urandom of="$seed" bs=1M count="$seed_mib" status=none
        while true; do
            dd if="$seed" of="$file" bs=1M conv=fsync status=none
            sleep 0.5
        done &
        loader=$!
        # shellcheck disable=SC2064 # The loader and its file as they are now.
        trap "kill $loader; wait $loader 2> /dev/null || true; rm -f '$file'" EXIT
    fi
    "$record" run "$out" -- bash "${BASH_SOURCE[0]}" work "$out" "$secs"
}

case ${1:-} in
    work) work "$2" "$3"; exit ;;
    round) shift; round "$@"; exit ;;
esac
(( $# == 1 )) || { echo "usage: contamination-check.sh RESULTS" >&2; exit 2; }
command -v fxjob > /dev/null ||
    { echo "fxjob not found: this runs on a host with fxjob (ubu1)" >&2; exit 2; }
[[ ! -e $1 ]] || { echo "$1 exists: name a new directory" >&2; exit 2; }
secs=${CLADEFS_CONTAMINATION_CHECK_SECS:-90}
[[ $secs =~ ^[1-9][0-9]*$ ]] ||
    { echo "CLADEFS_CONTAMINATION_CHECK_SECS is a count of seconds" >&2; exit 2; }
load=${CLADEFS_CONTAMINATION_CHECK_LOAD:-$HOME}
mkdir -p "$1"
results=$(cd "$1" && pwd)

fxjob --bench --mem 2G --name contamination-check-alone -- \
    bash "${BASH_SOURCE[0]}" round "$results/alone" "$secs"
fxjob --bench --mem 2G --name contamination-check-beside -- \
    bash "${BASH_SOURCE[0]}" round "$results/beside" "$secs" "$load"

failures=0
# check WHAT TEXT PATTERN: that TEXT matches PATTERN, a glob.
check() {
    # shellcheck disable=SC2053 # A glob, to match.
    if [[ $2 == $3 ]]; then echo "ok: $1"; else
        echo "FAILED: $1: expected $3, got: $2" >&2
        failures=$((failures + 1))
    fi
}
alone=$("$record" said "$results/alone") beside=$("$record" said "$results/beside")
echo "alone: $alone"
echo "beside: $beside"
check "the round alone is clean" "$alone" "clean: others wrote * MB/s to its disk over *"
check "its own I/O was a cgroup's" \
    "$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["cgroup"])' \
        "$results/alone/contamination.json")" "/*"
check "the round beside the loader is contaminated" "$beside" \
    "CONTAMINATED: others wrote * MB/s to its disk over the round, bound 10 (*"
status=0
"$repo/scripts/bench-ab.py" alone "$results/alone" -- beside "$results/beside" \
    > "$results/ab.md" 2> "$results/ab.err" || status=$?
cat "$results/ab.md"
check "the comparison fails" "$status" 1
check "saying why" "$(< "$results/ab.err")" "NO RESULT: beside has no clean round of: pass"
check "the loaded round's timings are left out" "$(grep -F '| pass | s |' "$results/ab.md")" \
    "| pass | s | * | NO RESULT \[contaminated rounds left out: 1\] | - | NO RESULT |"
check "its counts are kept" "$(grep -F '| written | MiB |' "$results/ab.md")" \
    "| written | MiB | * (*) | * (*) | *× | * |"
if (( failures )); then
    echo "contamination-check: $failures failed; the records are in $results" >&2
    exit 1
fi
echo "contamination-check: all passed"
