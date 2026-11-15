#!/bin/bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Puts each of crates/*/loom-mutants/*.patch into the tree, an ordering weakened (Release or Acquire
# to Relaxed) or a bug a loom model found put back, runs the loom model its header names
# (`# Killer: CRATE TEST`, of the crate's library, with its `loom` feature) against it, and takes
# it out: killed if the model fails, MISSED if it passes, BROKEN if the patch doesn't apply or the
# tree doesn't build. A mutant whose header says `# Fate: equivalent`, with why, is one no model
# can kill, as what it weakens orders nothing another ordering doesn't (a site stronger than
# needed): it is to pass, and fails the run if the model fails, as its why is then wrong. This
# fails unless each has its fate (docs/memory-ordering.md, TASK-444.2). Runs anywhere loom does,
# in seconds a mutant beside its build.
#
#   loom-mutants.sh [--check] [NAME...]
#
# Each mutant's log is kept in $CLADEFS_MUTANTS_LOGS (default: a new directory, which it names). The
# tree has the mutant in it while it runs. Each patch applies as it is, with no fuzz. --check runs
# nothing: it applies and reverses each patch in a copy of what it changes, as CI does.

set -u -o pipefail
cd "$(dirname "$0")/.."

check=""
if [[ ${1:-} == --check ]]; then
    check=1
    shift
fi

patches=()
if [[ $# == 0 ]]; then
    patches=(crates/*/loom-mutants/*.patch)
else
    for name in "$@"; do
        found=(crates/*/loom-mutants/"$name".patch)
        [[ -f ${found[0]} ]] || { echo "no loom mutant $name" >&2; exit 2; }
        patches+=("${found[0]}")
    done
fi

# patch_round_trip (scripts/lib/patches.sh): whether a patch is well formed, applies and reverses.
source scripts/lib/patches.sh

broken=0
if [[ -n $check ]]; then
    for patch in "${patches[@]}"; do
        patch_round_trip "$patch" || broken=$((broken + 1))
        read -r crate test <<< "$(sed -n 's/^# Killer: //p' "$patch")"
        [[ -n ${test:-} ]] || { echo "$patch names no killer" >&2; broken=$((broken + 1)); }
    done
    echo "loom mutants: ${#patches[@]} checked, $broken broken"
    exit $((broken > 0))
fi

failed=0
logs=${CLADEFS_MUTANTS_LOGS:-$(mktemp -d)}
mkdir -p "$logs"
echo "logs: $logs"
printf '%-48s %-8s %s\n' mutant time fate
for patch in "${patches[@]}"; do
    name=$(basename "$patch" .patch)
    read -r crate test <<< "$(sed -n 's/^# Killer: //p' "$patch")"
    equivalent=$(sed -n '/^# Fate: equivalent/{s/.*/yes/p;q;}' "$patch")
    if ! patch_round_trip "$patch" || [[ -z ${test:-} ]]; then
        printf '%-48s %-8s %s\n' "$name" - "BROKEN: its patch doesn't apply, or names no killer"
        failed=1
        continue
    fi
    patch -s -F0 -p1 < "$patch"
    log=$logs/$name.log
    began=$SECONDS
    cargo nextest run -p "$crate" --features loom --lib --no-fail-fast -E "test(=$test)" \
        > "$log" 2>&1
    status=$?
    patch -s -F0 -R -p1 < "$patch" || { echo "$name: its patch doesn't reverse: the tree has it" >&2; exit 1; }
    # Its files rewritten, as patch writes them: cargo builds them anew, never the mutant again.
    sed -n 's|^+++ b/||p' "$patch" | xargs touch
    case $status:$equivalent in
        0:yes) fate="survived, as equivalent" ;;
        0:) fate=MISSED; failed=1 ;;
        100:yes) fate="KILLED, though said equivalent"; failed=1 ;;
        100:) fate="killed: $(sed -n '/panicked at/{n;s/^ *//p;}' "$log" | head -1)" ;;
        *) fate="BROKEN: exit $status: $(sed -n '/^error/{p;q;}' "$log")"; failed=1 ;;
    esac
    printf '%-48s %-8s %s\n' "$name" "$((SECONDS - began))s" "${fate:0:160}"
done
exit $failed
