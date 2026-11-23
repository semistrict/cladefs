#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Checks, in a hotbed workspace on a test host, that a measurement's round says when others wrote
# beside it (scripts/contamination.py, TASK-375): two rounds, each a measurement of its own (hotbed
# run --bench), one alone, one beside a disk loader, then scripts/bench-ab.py's comparison of the
# two. It takes about four minutes once the measurements' slot is free.
#
#   scripts/contamination-check.sh NAME
#
# It runs from a checkout of cladefs, in the session hotbed binds to it, on CLADEFS_TEST_HOST's
# node, which it needs named, the rounds and their records in the workspace's
# target/contamination-check/NAME (board 6058). A round here is no guest's: for
# CLADEFS_CONTAMINATION_CHECK_SECS seconds (default 90) it rewrites a 64 MiB file with fsync in its
# command's TMPDIR, the workspace's, and records each pass's time (`pass`, s) and what it wrote
# (`written`, MiB), as a case does (NAME/ROUND/bench-check.json). The loader rewrites a 64 MiB file
# with fsync twice a second, 128 MB/s at most, in the workspace too, as a command of its own, of
# work, started before the round beside it and stopped after: another cgroup's writes to the
# round's disk, which the round's record must count as others'.
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

# round OUT SECS: a round, in its command: the workload under the record.
round() {
    mkdir -p "$1"
    "$record" run "$1" -- bash "${BASH_SOURCE[0]}" work "$1" "$2"
}

# loader DIR: rewrites a 64 MiB file with fsync in DIR twice a second, until it is stopped.
loader() {
    local seed=$TMPDIR/contamination-check-load file=$1/contamination-check-loader
    mkdir -p "$1"
    dd if=/dev/urandom of="$seed" bs=1M count="$seed_mib" status=none
    trap 'rm -f "$file"' EXIT
    while true; do
        dd if="$seed" of="$file" bs=1M conv=fsync status=none
        sleep 0.5
    done
}

case ${1:-} in
    work) work "$2" "$3"; exit ;;
    round) round "$2" "$3"; exit ;;
    loader) loader "$2"; exit ;;
    check) results=$2 ;;
    *)
        (( $# == 1 )) && [[ $1 =~ ^[A-Za-z0-9._-]+$ ]] ||
            { echo "usage: contamination-check.sh NAME" >&2; exit 2; }
        node=${CLADEFS_TEST_HOST:?CLADEFS_TEST_HOST names the hotbed node the rounds run on}
        secs=${CLADEFS_CONTAMINATION_CHECK_SECS:-90}
        [[ $secs =~ ^[1-9][0-9]*$ ]] ||
            { echo "CLADEFS_CONTAMINATION_CHECK_SECS is a count of seconds" >&2; exit 2; }
        dir=${CLADEFS_HOTBED_WORK:-/work}/${CLADEFS_HOTBED_REPO:-cladefs}/target/contamination-check/$1
        me=scripts/contamination-check.sh
        cd "$repo"
        # The checkout, synced once; then the rounds, each a measurement, and the loader beside
        # the second, a command of work, all in the one workspace on the node.
        hotbed run --node "$node" --kind shell -- bash -c \
            "if [ -e $dir ]; then echo '$dir exists: name another' >&2; exit 2; fi; mkdir -p $dir"
        hotbed run --no-sync --node "$node" --bench -- bash "$me" round "$dir/alone" "$secs"
        loading=$(hotbed run --no-sync --node "$node" --detach --json -- bash "$me" loader "$dir/load" |
            python3 -c 'import json, sys; print(json.load(sys.stdin)["data"]["id"])')
        status=0
        hotbed run --no-sync --node "$node" --bench -- bash "$me" round "$dir/beside" "$secs" ||
            status=$?
        hotbed stop "$loading" > /dev/null
        (( status == 0 )) || exit "$status"
        exec hotbed run --no-sync --node "$node" --kind check -- bash "$me" check "$dir"
        ;;
esac

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
