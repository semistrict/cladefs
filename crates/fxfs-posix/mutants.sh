#!/bin/bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Puts each of mutants/*.patch into the tree, a bug of the kind the model test exists for, runs the
# crate's tests against it, and takes it out: which test finds it, how soon, or that none does.
#
#   mutants.sh [--check] [-c CASES] [NAME...]
#
# CASES is how many sequences the model test tries of each mutant: what it finds, it finds without
# shrinking. A mutant only that finds, and by chance, wants a test that always does: run this at the
# suite's own 96 to see. The tree has the mutant in it while it runs, and as it was when it ends,
# stopped or not. Each patch applies as it is, with no fuzz: one that needs any has drifted from the
# code it changes. --check runs nothing: it applies and reverses each patch in a copy of what it
# changes, in a second, as CI does. A patch that doesn't apply fails either; so does a mutant whose
# tests couldn't run (they didn't build, or cargo didn't start: exit 127 on a host whose
# non-login shell has no cargo), BROKEN, as it was tried by nothing (board 2718).

set -u -o pipefail
cd "$(dirname "$0")/../.."

check=""
if [[ ${1:-} == --check ]]; then
    check=1
    shift
fi
cases=96
while getopts c: opt; do
    case $opt in
        c) cases=$OPTARG ;;
        *) exit 2 ;;
    esac
done
shift $((OPTIND - 1))

mutants=crates/fxfs-posix/mutants
# patch_round_trip (scripts/lib/patches.sh): whether a patch is well formed, applies and reverses;
# patch_run, a command run with it in the tree.
source scripts/lib/patches.sh
mutants_names "$mutants" "$@"

if [[ -n $check ]]; then
    mutants_check "$mutants" "${names[@]}"
fi

broken=0

printf '%-42s %-8s %s\n' mutant time 'found by'
for name in "${names[@]}"; do
    patch=$mutants/$name.patch
    if ! patch_round_trip "$patch"; then
        printf '%-42s %-8s %s\n' "$name" - "BROKEN: its patch doesn't apply"
        broken=1
        continue
    fi
    log=$(mktemp)
    began=$SECONDS
    patch_run "$patch" "$log" env CLADEFS_MODEL_CASES="$cases" PROPTEST_MAX_SHRINK_ITERS=0 cargo nextest run -p fxfs-posix
    status=$?
    case $status in
        0) found='not caught' ;;
        100) found=$(grep -m1 -E '^ *(FAIL|SIGABRT|TIMEOUT) ' "$log" | sed 's/.*fxfs-posix //') ;;
        *) found="BROKEN: exit $status: $(grep -m1 -E '^error' "$log")"; broken=1 ;;
    esac
    printf '%-42s %-8s %s\n' "$name" "$((SECONDS - began))s" "$found"
    rm -f "$log"
done
exit $broken
