#!/bin/bash
# SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
# Copyright 2026 The fxdax Authors.
#
# Traces of tests/traced's seeds as specs/dax-cow's actions (TASK-444.1, kernel/spec_trace.c),
# for specs/dax-cow/trace/TraceDaxCow.tla to follow (scripts/check-tla.sh, on any machine with
# Java): OUT/seed-N/TraceDaxCowData.tla, each a run that passed; and, with -m, each past bug of
# ../mutants put back, OUT/mutant-NAME-seed-N/, each run of it whether or not it failed, with
# what it said in OUT/mutant-NAME-seed-N/run.log. Takes KERNEL and BUILD as the Makefile does.
#
#   trace.sh [-s SEEDS] [-f FIRST] [-o OPERATIONS] [-d OUT] [-m MUTANT]...

set -u -o pipefail
cd "$(dirname "$0")"

first=1 seeds=12 operations=60 out="" mutants=()
while getopts s:f:o:d:m: opt; do
    case $opt in
        s) seeds=$OPTARG ;;
        f) first=$OPTARG ;;
        o) operations=$OPTARG ;;
        d) out=$OPTARG ;;
        m) mutants+=("$OPTARG") ;;
        *) exit 2 ;;
    esac
done
. ./lib.sh
sim_prepare
out=$(realpath -m "${out:-$BUILD/traces}")
mkdir -p "$out"

# Traces `bin`'s seeds into OUT/PREFIXseed-N, of SOURCES: fails if a run of the tree's fails.
trace() { # bin, prefix, sources, must-pass
    local bin=$1 prefix=$2 sources=$3 must=$4 seed dir status=0
    for ((seed = first; seed < first + seeds; seed++)); do
        dir=$out/${prefix}seed-$seed
        mkdir -p "$dir"
        if ! SIM_TRACE_SOURCES=$sources timeout 120 "$bin" "$seed" "$operations" \
            -o "$dir/TraceDaxCowData.tla" > "$dir/run.log" 2>&1; then
            echo "${prefix}seed $seed: $(sim_failure "$dir/run.log")"
            if [[ $must == 1 ]]; then
                status=1
            fi
        fi
    done
    return $status
}

status=0
trace "$BUILD/tests/traced" "" "$(./trace-sources.sh)" 1 || status=1
for name in "${mutants[@]}"; do
    patch=../mutants/$name.patch
    sim_build "$name" "$patch" > "$out/$name-build.log" 2>&1 ||
        { echo "$name: it doesn't apply or build: $out/$name-build.log"; status=1; continue; }
    trace "$BUILD/mutants/$name/traced" "mutant-$name-" "$(./trace-sources.sh "$patch")" 0
done
echo "trace: seeds $first to $((first + seeds - 1)) at $operations operations, in $out"
exit $status
