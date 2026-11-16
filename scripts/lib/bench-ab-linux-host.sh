#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# An A/B of the fs bench (scripts/bench.sh) on an x86-64 Linux KVM host, as root
# (scripts/bench-ab-host.sh drives it, TASK-276): each side's tree built once, in a tree of its own
# (host_tree), into artifacts of its own, the guest kernel among them (test-e2e.sh's
# CLADEFS_E2E_BUILD_ONLY); then ROUNDS rounds, each of both sides, A B, then B A, and so on, each a
# run of the bench against its side's artifacts, building nothing (CLADEFS_E2E_PREBUILT), so that
# what drifts on the host moves both alike, and a round takes the workloads' time alone. Each
# round's results go to RESULTS/NAME-rN, and scripts/bench-ab.py's comparison of the sides to
# RESULTS/ab.md, the control CLADEFS_AB_CONTROL's case (default ext4-guest-kernel; empty: none).
# Each round records what others wrote to its disk as it ran (RESULTS/NAME-rN/contamination.json,
# scripts/contamination.py, whose bounds CLADEFS_CONTAMINATION_* set), and the comparison leaves the
# timings of a round others wrote beside out, saying so (TASK-375).
#
# With CLADEFS_AB_SUITE=qualify (default fs, the bench), each round is the qualification instead
# (scripts/qualify.sh, TASK-330.4), of LangSmith's pin against a candidate: its harness, the
# runner and the guest agent, this tree's for both sides (test-e2e.sh's CLADEFS_E2E_HARNESS), as a
# pin from before the qualification's cases lacks them; what each side builds, its extent daemon,
# its Firecracker and its guest kernel, what it tests. No control by default: what moves, and
# each side's failed checks, ab.md says (scripts/bench-ab.py).
#
# Usage: bench-ab-linux-host.sh TOOLS RESULTS A_SOURCE B_SOURCE ROUNDS A_NAME B_NAME
#
# CLADEFS_TEST_HOST_PHASE (linux-host-setup.sh's host_phase) is what of it runs: `build` builds the
# sides (and the qualification's harness) into the work directory, and stops; `measure` runs the
# rounds of what a build there made, and compares them; unset, both. scripts/bench-ab-host.sh runs
# the build as work and the rounds alone as a measurement (fxjob --bench, scripts/lib/host.sh's
# host_job), so that compiling takes no measurement's cores (TASK-330.4, TASK-368).
#
# TOOLS is the tree whose scripts run, this one's: the sides' trees may be of before them (an
# A/B's base), and build and boot their own code (test-e2e.sh's CLADEFS_E2E_ROOT). B_SOURCE `same`
# is A's tree, built once, for an A/B of one build that CLADEFS_AB_A_ENV and CLADEFS_AB_B_ENV tell
# apart: NAME=VALUE words each side's rounds run with, such as
# CLADEFS_GUEST_CMDLINE=virtio_extent.affine=0. CLADEFS_BENCH_CASES, CLADEFS_BENCH_GROUPS,
# CLADEFS_BENCH_ITERATIONS (each round's, default 2) and the like are the same for both sides.
# CLADEFS_GUEST_KERNEL_BUILD is the guest kernels' build (default release).
#
# For the nightly (scripts/nightly-host.sh, TASK-441.1), whose baseline is the commit the night
# before measured: CLADEFS_AB_KEEP names a directory where each side's artifacts are kept, by its
# source revision and guest kernel build (ab_key), so that a side built by a run before builds
# nothing, its kept artifacts what its rounds run; the CLADEFS_AB_KEEP_COUNT (default 4) used last
# are kept, the others removed. CLADEFS_AB_COUNTS=1 runs scripts/counts.sh (this tree's) on each
# side as it is built, the exact counts kept with its artifacts, and copied to RESULTS/NAME-counts.
# CLADEFS_AB_RERUN=1 runs each round scripts/ab-rounds.py leaves out (others wrote beside it, or
# its control moved) once more, of both sides in the same order, as round ROUNDS+K, which
# RESULTS/reruns.txt records (`RERUN ROUND`). A side that fails to build says so in
# RESULTS/build-failed, its name, and the run ends there.
set -euo pipefail

# Prints the sides' order of each of ROUNDS rounds, a line each: `a b`, then `b a`, and so on, so
# that neither side always runs first, after the other's warming of the host.
ab_order() {
    local r
    for (( r = 1; r <= $1; r++ )); do
        if (( r % 2 )); then echo "a b"; else echo "b a"; fi
    done
}

# ab_key TREE prints what a side's artifacts are kept by in CLADEFS_AB_KEEP: TREE's source
# revision (its source-revision.txt, scripts/lib/host-remote.sh's) and the guest kernel's build;
# nothing for a tree whose revision doesn't name its source alone (changed from its commit, or
# unknown), whose artifacts are not kept.
ab_key() {
    local revision
    revision=$(sed -n 1p "$1/source-revision.txt" 2> /dev/null) || true
    [[ $revision =~ ^[0-9a-f]{40}$ ]] || return 0
    echo "$revision-$kernel_build"
}

# Each side's tree and artifacts, tree_a and artifacts_a, tree_b and artifacts_b, which build makes
# or finds kept; `of tree SIDE` and `of artifacts SIDE` print them.
tree_a='' tree_b='' artifacts_a='' artifacts_b=''
of() {
    local name="$1_$2"
    printf '%s' "${!name}"
}

# build SIDE SOURCE builds side SIDE from SOURCE once: its tree, tree_SIDE, its guest kernel and
# its artifacts, artifacts_SIDE (host_artifacts), and, with CLADEFS_AB_COUNTS=1, its exact counts
# in the artifacts' counts (scripts/counts.sh). With CLADEFS_AB_KEEP, artifacts kept of the same
# key (ab_key) are the side's, built by a run before, and nothing builds; what this builds is kept
# there. A side that fails to build is named in RESULTS/build-failed, and fails this.
build() {
    local side=$1 source=$2 key='' kept='' made
    repo=$source
    host_tree "bench-ab-$side"
    printf -v "tree_$side" '%s' "$repo"
    if [[ -n ${CLADEFS_AB_KEEP:-} ]]; then key=$(ab_key "$repo"); fi
    if [[ -n $key ]]; then kept=$CLADEFS_AB_KEEP/$key; fi
    if [[ -n $kept && -f $kept/artifacts.txt ]]; then
        echo "Side $side's artifacts kept from a run before: $kept; building nothing" >&2
        touch "$kept"
    else
        echo "Building side $side from $source" >&2
        made=$work/ab-artifacts-$side
        if ! host_kernel_log=$results/guest-kernel-$side.log \
            host_kernel "$kernel_build" "$work/guest-kernel-ab-$side-$kernel_build" ||
            ! host_artifacts "$made" "$side" "$host_kernel_image"; then
            echo "$side" > "$results/build-failed"
            echo "Side $side failed to build: see $results/build-$side.log and" \
                "$results/guest-kernel-$side.log" >&2
            return 1
        fi
        if [[ -n $kept ]]; then
            mkdir -p "$CLADEFS_AB_KEEP"
            if [[ -e $kept.new ]]; then rm -r -- "$kept.new"; fi
            cp -a "$made" "$kept.new"
            mv "$kept.new" "$kept"
            touch "$kept"
            rm -r -- "$made"
            echo "Side $side's artifacts kept in $kept" >&2
        else
            kept=$made
        fi
    fi
    printf -v "artifacts_$side" '%s' "$kept"
    cp "$kept/artifacts.txt" "$results/artifacts-$side.txt"
    if [[ ${CLADEFS_AB_COUNTS:-} == 1 ]]; then
        if [[ ! -f $kept/counts/status.txt ]]; then
            echo "Counting side $side's exact counts (scripts/counts.sh)" >&2
            "$host_tools/scripts/counts.sh" "$repo" "$kept/counts.new" "$work/counts-$side" \
                > "$results/counts-$side.log" 2>&1 ||
                { echo "side $side's counts could not start: see $results/counts-$side.log" >&2
                  return 1; }
            mv "$kept/counts.new" "$kept/counts"
        fi
    fi
    ab_prune
}

# ab_prune removes from CLADEFS_AB_KEEP all but the CLADEFS_AB_KEEP_COUNT (default 4) artifacts
# used last, but never one a side of this run uses.
ab_prune() {
    local keep=${CLADEFS_AB_KEEP_COUNT:-4} old side used
    [[ -n ${CLADEFS_AB_KEEP:-} && -d $CLADEFS_AB_KEEP ]] || return 0
    [[ $keep =~ ^[1-9][0-9]*$ ]] || { echo "CLADEFS_AB_KEEP_COUNT is a count" >&2; return 2; }
    # shellcheck disable=SC2012 # Its names are ab_key's, hexadecimal and a word: by time, newest first.
    while read -r old; do
        used=0
        for side in a b; do
            if [[ $(of artifacts "$side") == "$CLADEFS_AB_KEEP/$old" ]]; then used=1; fi
        done
        if (( ! used )); then
            echo "Removing kept artifacts $CLADEFS_AB_KEEP/$old" >&2
            rm -r -- "${CLADEFS_AB_KEEP:?}/$old"
        fi
    done < <(ls -1t "$CLADEFS_AB_KEEP" | sed -n "$((keep + 1)),\$p")
}

# Sourced, as by the scripts' tests, it defines its functions and does nothing else.
[[ ${BASH_SOURCE[0]} == "$0" ]] || return 0

tools=$(cd "${1:?tools required}" && pwd)
results=${2:?results directory required}
a_source=${3:?A source required} b_source=${4:?B source required}
rounds=${5:?rounds required} a_name=${6:?A name required} b_name=${7:?B name required}
[[ $rounds =~ ^[1-9][0-9]*$ ]] || { echo "ROUNDS is a count" >&2; exit 2; }
repo=$tools
source "$(dirname "${BASH_SOURCE[0]}")/linux-host-setup.sh"
host_setup
kernel_build=${CLADEFS_GUEST_KERNEL_BUILD:-release}
[[ $kernel_build == release || $kernel_build == debug ]] ||
    { echo "CLADEFS_GUEST_KERNEL_BUILD is release or debug" >&2; exit 2; }
unset CLADEFS_GUEST_KERNEL_BUILD
iterations=${CLADEFS_BENCH_ITERATIONS:-2}
suite=${CLADEFS_AB_SUITE:-fs}
[[ $suite == fs || $suite == qualify ]] || { echo "CLADEFS_AB_SUITE is fs or qualify" >&2; exit 2; }
read -ra a_env <<< "${CLADEFS_AB_A_ENV:-}"
read -ra b_env <<< "${CLADEFS_AB_B_ENV:-}"


# What a build made, which the rounds run: each side's tree, and the harness.
built=$work/ab-built
harness=

# Builds both sides (build), and the qualification's harness, this tree's, the same for both
# sides, which a side of this tree's own revision built already; and records what it made, in
# $built and the results' host.txt.
build_sides() {
    local side name
    build a "$a_source"
    if [[ $b_source == same ]]; then
        tree_b=$tree_a artifacts_b=$artifacts_a
        cp "$results/artifacts-a.txt" "$results/artifacts-b.txt"
    else
        build b "$b_source"
    fi
    for side in a b; do
        if [[ -d $(of artifacts "$side")/counts ]]; then
            name=$a_name
            if [[ $side == b ]]; then name=$b_name; fi
            cp -a "$(of artifacts "$side")/counts" "$results/$name-counts"
        fi
    done

    if [[ $suite == qualify ]]; then
        for side in a b; do
            if cmp -s "$tools/source-revision.txt" "$(of tree "$side")/source-revision.txt"; then
                harness=$(of artifacts "$side")
                break
            fi
        done
        if [[ -z $harness ]]; then
            harness=$work/ab-harness
            echo "Building the harness from $tools" >&2
            repo=$tools host_artifacts "$harness" harness
        fi
        cp "$harness/artifacts.txt" "$results/artifacts-harness.txt"
    fi

    {
        host_record
        echo "Suite $suite${harness:+, the harness of $(sed -n 's/^source //p' "$harness/artifacts.txt")}"
        echo "A $a_name: $a_source ${a_env[*]:-}"
        echo "B $b_name: $b_source ${b_env[*]:-}"
    } > "$results/host.txt"
    printf 'a %s %s\nb %s %s\nharness %s\n' "$tree_a" "$artifacts_a" "$tree_b" "$artifacts_b" \
        "$harness" > "$built"
}

# Reads what a build made from $built.
load_build() {
    local key value made
    [[ -f $built ]] || { echo "$work holds no build: CLADEFS_TEST_HOST_PHASE=build first" >&2; exit 2; }
    while read -r key value made; do
        case $key in
            a | b) printf -v "tree_$key" '%s' "$value"; printf -v "artifacts_$key" '%s' "$made" ;;
            harness) harness=$value ;;
        esac
    done < "$built"
}

if [[ $host_phase == measure ]]; then
    load_build
else
    build_sides
fi
if [[ $host_phase == build ]]; then
    echo "Built in ${SECONDS}s." | tee -a "$results/host.txt"
    exit 0
fi

# Runs side SIDE's round ROUND, with its name and its environment's words, its results in
# RESULTS/NAME-rROUND, what others wrote to its disk meanwhile among them
# (scripts/contamination.py, TASK-375), which scripts/bench-ab.py leaves a round's timings out for.
round() {
    local side=$1 round=$2 name out
    local -a env command
    if [[ $side == a ]]; then name=$a_name env=(${a_env[@]+"${a_env[@]}"}); else
        name=$b_name env=(${b_env[@]+"${b_env[@]}"}); fi
    out=$results/$name-r$round
    env+=("CLADEFS_E2E_PREBUILT=$(of artifacts "$side")"
        "CLADEFS_GUEST_KERNEL=$(of artifacts "$side")/vmlinux")
    if [[ $suite == qualify ]]; then
        env+=("CLADEFS_E2E_HARNESS=$harness")
        command=("$tools/scripts/qualify.sh" "$out")
    else
        env+=("CLADEFS_E2E_ROOT=$(of tree "$side")" "CLADEFS_BENCH_RESULTS=$out")
        command=("$tools/scripts/bench.sh" "$iterations")
    fi
    echo "Round $round: $name" >&2
    env "${env[@]}" "$tools/scripts/contamination.py" run "$out" -- "${command[@]}" \
        > "$out.md" 2> "$out.log"
}

# A round of the qualification one of whose parts didn't run (qualify.sh's 3) ends the A/B there:
# what follows would compare nothing for that part, for hours (TASK-330.4).
status=0 r=0
while read -r first second; do
    r=$((r + 1))
    for side in "$first" "$second"; do
        ran=0
        round "$side" "$r" || ran=$?
        if (( ran )); then status=1; fi
        if [[ $suite == qualify ]] && (( ran == 3 )); then
            echo "Round $r: side $side didn't run every part of the qualification: the A/B stops" \
                "(see its log)" >&2
            break 2
        fi
    done
done < <(ab_order "$rounds")

control=${CLADEFS_AB_CONTROL-ext4-guest-kernel}
if [[ $suite == qualify ]]; then control=${CLADEFS_AB_CONTROL-}; fi

# Each round left out, rerun once, as the next round's number, of both sides in the round's own
# order (TASK-441.1).
ran=$rounds
if [[ ${CLADEFS_AB_RERUN:-} == 1 && $suite == fs ]]; then
    mapfile -t again < <("$tools/scripts/ab-rounds.py" discarded --control "$control" \
        "$results" "$a_name" "$b_name" 2>> "$results/reruns.log")
    for r in ${again[@]+"${again[@]}"}; do
        ran=$((ran + 1))
        echo "Round $ran: round $r again, which was left out (see $results/reruns.log)" >&2
        echo "$ran $r" >> "$results/reruns.txt"
        read -r first second < <(ab_order "$r" | sed -n "${r}p")
        for side in "$first" "$second"; do
            round "$side" "$ran" || status=1
        done
    done
fi

sides=()
for (( r = 1; r <= ran; r++ )); do sides+=("$results/$a_name-r$r"); done
sides+=(-- "$b_name")
for (( r = 1; r <= ran; r++ )); do sides+=("$results/$b_name-r$r"); done
"$tools/scripts/bench-ab.py" ${control:+--control "$control"} "$a_name" "${sides[@]}" \
    > "$results/ab.md" || status=1
# The guests' root filesystems and the stores, which a failed case leaves, are not results.
find "$results" -type d \( -name rootfs -o -name extents -o -name '*.img' \) -prune \
    -exec rm -rf {} +
find "$results" -type f \( -name '*.img' -o -name rootfs.ext4 \) -delete
echo "Took ${SECONDS}s." | tee -a "$results/host.txt"
cat "$results/ab.md"
exit "$status"
