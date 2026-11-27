#!/bin/bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Puts each of mutants/*.patch into the tree, a bug of a kind the simulation of bucket_cell's
# protocol exists to find (TASK-149.32: an object an acknowledged segment names gone, an upload never
# woken), runs the simulation's test its header names (`# Killer: TEST`, local_cell's, TASK-88,
# built with the profile sim, as scripts/test.sh's sim part builds it) against it, and takes it out: killed if the test fails, MISSED if it passes,
# BROKEN if the patch doesn't apply or the tree doesn't build. This fails unless each is killed.
#
#   mutants.sh [--check] [NAME...]
#
# Each mutant's log is kept in $CLADEFS_MUTANTS_LOGS (default: a new directory, which it names). The
# tree has the mutant in it while it runs, and as it was when it ends, stopped or not. Each patch
# applies as it is, with no fuzz: one that needs any has drifted from the code it changes. --check
# runs nothing: it applies and reverses each patch in a copy of what it changes, in a second, as CI
# does. A patch that doesn't apply fails either.

set -u -o pipefail
cd "$(dirname "$0")/../.."

check=""
if [[ ${1:-} == --check ]]; then
    check=1
    shift
fi

mutants=crates/bucket_cell/mutants
# patch_round_trip (scripts/lib/patches.sh): whether a patch is well formed, applies and reverses;
# patch_run, a command run with it in the tree.
source scripts/lib/patches.sh
mutants_names "$mutants" "$@"

if [[ -n $check ]]; then
    mutants_check "$mutants" --require-killer "${names[@]}"
fi

failed=0
logs=${CLADEFS_MUTANTS_LOGS:-$(mktemp -d)}
mkdir -p "$logs"
echo "logs: $logs"
printf '%-40s %-8s %s\n' mutant time fate
for name in "${names[@]}"; do
    patch=$mutants/$name.patch
    killer=$(sed -n 's/^# Killer: //p' "$patch")
    if ! patch_round_trip "$patch" || [[ -z $killer ]]; then
        printf '%-40s %-8s %s\n' "$name" - "BROKEN: its patch doesn't apply, or names no killer"
        failed=1
        continue
    fi
    log=$logs/$name.log
    began=$SECONDS
    patch_run "$patch" "$log" cargo nextest run --cargo-profile sim -p local_cell --no-fail-fast -E "test(=$killer)"
    status=$?
    case $status in
        0) fate=MISSED; failed=1 ;;
        # What failed: the simulation's first violation of the seed that found it.
        100) fate="killed: $(sed -n 's/^ *seed \([0-9]*\): \[$/seed \1/p; s/^ *"\(.*\)",\{0,1\}$/\1/p' "$log" | head -2 | paste -sd ' ' -)" ;;
        *) fate="BROKEN: exit $status: $(sed -n '/^error/{p;q;}' "$log")"; failed=1 ;;
    esac
    printf '%-40s %-8s %s\n' "$name" "$((SECONDS - began))s" "${fate:0:160}"
done
exit $failed
