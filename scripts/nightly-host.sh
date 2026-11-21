#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# The nightly on the test host (TASK-441): main's head measured against the commit the night before
# measured, in interleaved rounds of the same night, inside one fxjob --bench job, and what moved
# filed as tasks on the board (BOARD_URL), pushed to main. A cron entry runs it at 02:00 (scripts/nightly-install.sh),
# from the nightly's own clone of cladefs (CLADEFS_NIGHTLY_REPO, default ~/src/cladefs-nightly),
# which it alone uses: first it takes origin's main there (scripts/nightly.py sync: what an earlier
# night committed and could not push rebased on it and pushed) and runs main's own copy of itself.
#
#   scripts/nightly-host.sh
#
# Then, unless main's code is what the newest night measured (nightly.py plan: its head is main's
# newest commit that changed more than docs/measurements (and backlog/, where tasks were until
# 2026-11-21), which the nightly's own commits change; then it does nothing more), it runs scripts/bench-ab-host.sh on this test host
# itself (CLADEFS_TEST_HOST=local), side `base` the commit the night before measured, side `head`
# main's: each side built as ordinary work, but for the base, whose artifacts the night before kept,
# built when it was main's head (CLADEFS_AB_KEEP: a night builds one tree; both if they are gone),
# with its exact counts (scripts/counts.sh, CLADEFS_AB_COUNTS); then, alone in fxjob --bench,
# CLADEFS_NIGHTLY_ROUNDS rounds (3) of both sides interleaved, each CLADEFS_BENCH_ITERATIONS
# iterations (2) of the cases CLADEFS_NIGHTLY_CASES and groups CLADEFS_NIGHTLY_GROUPS (below), each
# round left out (others wrote beside it, or the control moved) run once more (CLADEFS_AB_RERUN). It
# may hold the bench slot for as long as it needs (CLADEFS_NIGHTLY_LIMIT, 8h). scripts/nightly.py
# report judges what came back (scripts/ab-rounds.py's bounds, CLADEFS_AB_*BOUND*), and publish
# commits it to main and pushes it. A side that failed to build, or a round that ran nothing, is a
# night that could not run: a result, said in the history page, not a crash; the next night measures
# the same commits again (nightly.py plan).
#
# The scratch disk, /mnt/fxdax-scratch, is every job's on the test host, and the first night found
# it full as its measurement began (TASK-489). So a night first removes its own earlier runs'
# directories, those of the nightly's name whose job is not running (scripts/lib/host.sh's
# CLADEFS_TEST_HOST_SWEEP; one night runs at a time, the lock below), and keeps only the artifacts
# the next night reuses, tonight's two sides' (CLADEFS_NIGHTLY_KEEP_COUNT, 2: tonight's head is its
# base, and a night that measured nothing is measured again); then checks the disk has the room it
# needs before it builds and before it measures (nightly.py room: CLADEFS_NIGHTLY_ROOM_BUILD and
# CLADEFS_NIGHTLY_ROOM_MEASURE, or more if earlier nights took more), and, short of it, is a night
# that could not run, its report naming the disk, the free space found and the need.
#
# What it measures, by default: the fs bench's cases that an agent sandbox's work stands on, the
# guest's Fxfs with DAX (sandboxes run DAX): its io group (cold and warm reads, sequential and
# random, fsynced appends, SQLite's scans), metadata's files and fsync parts (small files'
# create, stat, readdir, read, and create with fsync) and the sync group (sync, syncfs and freeze
# loops at a bucket's latency); a fork's boot from a base, cold and warm (the fork cases); and,
# as the control, ext4 on virtio-blk on the same guest kernel, no cladefs in it.
#
# Its log: ~/.local/state/cladefs/nightly/nightly.log (CLADEFS_NIGHTLY_STATE), a line a step, and
# each night's driver output in RESULTS/driver.log. A night's results: docs/measurements/
# HOST-nightly-<time>-<head>/ (nightly.md), and docs/measurements/nightly-history.md.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
state=${CLADEFS_NIGHTLY_STATE:-$HOME/.local/state/cladefs/nightly}
export CLADEFS_NIGHTLY_STATE=$state
mkdir -p "$state"

say() {
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*" | tee -a "$state/nightly.log" >&2
}

# One night at a time: a night still measuring at the next one's hour is left to finish.
if [[ -z ${CLADEFS_NIGHTLY_LOCKED:-} ]]; then
    exec 9> "$state/lock"
    if ! flock -n 9; then
        say "a night runs still: not starting another"
        exit 0
    fi
    export CLADEFS_NIGHTLY_LOCKED=1
fi

# Origin's main, and main's own copy of this script, once.
if [[ -z ${CLADEFS_NIGHTLY_SYNCED:-} ]]; then
    "$repo/scripts/nightly.py" --root "$repo" sync ||
        say "the clone is origin's main, but what an earlier night committed is not pushed" \
            "(see push-failed.txt)"
    export CLADEFS_NIGHTLY_SYNCED=1
    exec "$repo/scripts/nightly-host.sh" "$@"
fi

planned=$("$repo/scripts/nightly.py" --root "$repo" plan)
read -r what base head <<< "$planned"
if [[ $what == skip ]]; then
    say "nothing to measure: ${planned#skip }"
    exit 0
fi

# To the second, and made afresh: a night measured again (nightly.py plan) never shares its
# results with the one before, not even one of the same second and head, counted then (-2, -3):
# the test's nights, which measure nothing, follow each other within a second on CI's runners.
name=$repo/docs/measurements/$(hostname -s)-nightly-$(date -u +%Y-%m-%dT%H%M%SZ)-${head:0:8}
results=$name
for ((n = 2; ; n++)); do
    [[ -e $results ]] || break
    results=$name-$n
done
mkdir -p "$(dirname "$results")"
mkdir "$results"
say "measuring ${base:0:12} (base) against ${head:0:12} (head, main) into $results"
export CLADEFS_BENCH_CASES=${CLADEFS_NIGHTLY_CASES:-bench-fs-ext4-guest-kernel bench-fs-fxfs-guest-dax bench-fork-fxfs-guest-dax-cold bench-fork-fxfs-guest-dax-warm}
export CLADEFS_BENCH_GROUPS=${CLADEFS_NIGHTLY_GROUPS:-io,files,fsync,sync}
export CLADEFS_AB_ROUNDS=${CLADEFS_NIGHTLY_ROUNDS:-3}
export CLADEFS_BENCH_ITERATIONS=${CLADEFS_BENCH_ITERATIONS:-2}
rooms=$("$repo/scripts/nightly.py" --root "$repo" room)
read -r room room_measure <<< "$rooms"
say "the night needs $room bytes free on the scratch disk before building, $room_measure before" \
    "measuring"
status=0
CLADEFS_TEST_HOST=local CLADEFS_TEST_HOST_REPO=$repo CLADEFS_TEST_HOST_RUN=nightly \
    CLADEFS_TEST_HOST_SWEEP=1 \
    CLADEFS_TEST_HOST_ROOM=$room CLADEFS_TEST_HOST_ROOM_MEASURE=$room_measure \
    CLADEFS_TEST_HOST_LIMIT=${CLADEFS_NIGHTLY_LIMIT:-8h} \
    CLADEFS_AB_A=base=$base CLADEFS_AB_B=head=$head \
    CLADEFS_AB_KEEP=${CLADEFS_NIGHTLY_KEEP:-/mnt/fxdax-scratch/cladefs-nightly/artifacts} \
    CLADEFS_AB_KEEP_COUNT=${CLADEFS_NIGHTLY_KEEP_COUNT:-2} \
    CLADEFS_AB_COUNTS=1 CLADEFS_AB_RERUN=1 \
    "$repo/scripts/bench-ab-host.sh" "$results" > "$results/driver.log" 2>&1 || status=$?
say "the A/B ended $status"
if ! "$repo/scripts/nightly.py" --root "$repo" report --results "$results" --base "$base" \
    --head "$head" --status "$status"; then
    say "the night's report failed (above): its results are in $results, uncommitted"
    exit 1
fi
if ! "$repo/scripts/nightly.py" --root "$repo" publish --results "$results"; then
    say "the night's results are committed here, not pushed: the next night pushes them"
    exit 1
fi
