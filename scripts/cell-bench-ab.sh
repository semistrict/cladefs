#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# An A/B of cell-bench's cases that time a cell on a host's disk (`durable`, `burst`), two
# worktrees' builds of it, in a hotbed workspace on a test host: what a change to the cell's write
# path costs (TASK-539.4, the cells' files counted as they write them).
#
#   scripts/cell-bench-ab.sh build A_WORKTREE B_WORKTREE RESULTS
#   scripts/cell-bench-ab.sh measure A_WORKTREE B_WORKTREE RESULTS [ROUNDS]
#
# build, a hotbed command of work, builds each side (CARGO_BUILD_JOBS 4 unless the caller says),
# each into a target of its own, RESULTS/target-a and RESULTS/target-b: two worktrees sharing one
# link each other's crates (TASK-369). measure, a measurement (hotbed run --bench), runs ROUNDS
# rounds (6), each side's cases once a round, the sides' order reversed every other round (A B, B
# A, ...), so that what drifts on the host moves both alike. Their cells are in RESULTS/disk, on
# the workspace's filesystem (board 6058). Each case's output goes to RESULTS/SIDE/CASE-rN.txt, and
# RESULTS/summary.txt says, for each case's figures, each side's median over the rounds but the
# first, which runs cold.
set -euo pipefail

if [[ $# -lt 4 || ( $1 != build && $1 != measure ) ]]; then
    echo "usage: $0 build|measure A_WORKTREE B_WORKTREE RESULTS [ROUNDS]" >&2
    exit 2
fi
phase=$1
a=$(cd "$2" && pwd)
b=$(cd "$3" && pwd)
mkdir -p "$4"
results=$(cd "$4" && pwd)
rounds=${5:-6}
here=$(cd "$(dirname "$0")" && pwd)
source "$here/lib/measurement.sh"

if [[ $phase == build ]]; then
    measurement_refuses "cell-bench-ab.sh build"
    for side in a b; do
        tree=$a
        [[ $side == b ]] && tree=$b
        (
            cd "$tree"
            CARGO_TARGET_DIR="$results/target-$side" CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
                cargo build --release -p cladefs-examples --example cell-bench
        )
        mkdir -p "$results/$side"
        cp "$results/target-$side/release/examples/cell-bench" "$results/$side/cell-bench"
    done
    exit 0
fi

mount=$results/disk
mkdir -p "$mount"

run() {
    local side=$1 round=$2
    for case in durable burst; do
        TMPDIR=$mount "$results/$side/cell-bench" "$case" > "$results/$side/$case-r$round.txt"
    done
}
for round in $(seq 1 "$rounds"); do
    if (( round % 2 )); then
        run a "$round"
        run b "$round"
    else
        run b "$round"
        run a "$round"
    fi
done

# Each side's medians, round 1 left out.
python3 - "$results" "$rounds" > "$results/summary.txt" <<'EOF'
import re, statistics, sys
from pathlib import Path

results, rounds = Path(sys.argv[1]), int(sys.argv[2])
figures = {
    "durable": [("flush µs each", r"of them,\s+([\d.]+) µs each")],
    "burst": [
        ("write µs each", r"^writes: .*?([\d.]+) µs each"),
        ("ordered ms", r"ordered in ([\d.]+) ms"),
        ("durable ms", r"durable in ([\d.]+) ms"),
        ("framed ms", r"framed in ([\d.]+) ms"),
    ],
}
for case, named in figures.items():
    for name, pattern in named:
        medians = []
        for side in "ab":
            values = []
            for round in range(2, rounds + 1):
                text = (results / side / f"{case}-r{round}.txt").read_text()
                values.append(float(re.search(pattern, text, re.M).group(1)))
            medians.append(statistics.median(values))
        ratio = medians[1] / medians[0] if medians[0] else float("nan")
        print(f"{case} {name}: A {medians[0]:.1f}, B {medians[1]:.1f}, B/A {ratio:.3f}")
EOF
cat "$results/summary.txt"
