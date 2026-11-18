#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# An A/B of cell-bench's cases that time a store on a host's disk (`durable`, `burst`), two
# worktrees' builds of it, on this Linux host, a shared one (ubu1): what a change to the store's
# write path costs (TASK-539.4, the stores' files counted as they write them).
#
#   scripts/cell-bench-ab.sh A_WORKTREE B_WORKTREE RESULTS [ROUNDS]
#
# Each side is built first, as ordinary work (fxjob without --bench, niceness 19, CARGO_BUILD_JOBS
# 4 unless the caller says), each into a target of its own, RESULTS/target-a and RESULTS/target-b:
# two worktrees sharing one link each other's crates (TASK-369). Then one measurement, `fxjob
# --bench`, runs ROUNDS rounds (6), each side's cases once a round, the sides' order reversed every
# other round (A B, B A, ...), so that what drifts on the host moves both alike. Their stores are
# on ext4 over a RAM-backed loop device, made in the measurement (sudo) and gone with it, which
# supports O_DIRECT and leaves the host's disks out: the scratch disk, ext4 on a ZFS zvol, drifts
# up to four times within one job (TASK-433.8). Each case's output goes to RESULTS/SIDE/CASE-rN.txt,
# and RESULTS/summary.txt says, for each case's figures, each side's median over the rounds but the
# first, which runs cold.
set -euo pipefail

if [[ $# -lt 3 ]]; then
    echo "usage: $0 A_WORKTREE B_WORKTREE RESULTS [ROUNDS]" >&2
    exit 2
fi
a=$(cd "$1" && pwd)
b=$(cd "$2" && pwd)
mkdir -p "$3"
results=$(cd "$3" && pwd)
rounds=${4:-6}
here=$(cd "$(dirname "$0")" && pwd)

if [[ ${CELL_BENCH_AB_INNER:-} != 1 ]]; then
    for side in a b; do
        tree=$a
        [[ $side == b ]] && tree=$b
        (
            cd "$tree"
            fxjob --name "cell-bench-ab-build-$side" --mem 8G -- env \
                CARGO_TARGET_DIR="$results/target-$side" \
                CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
                nice -n 19 cargo build --release -p cladefs-examples --example cell-bench
        )
        mkdir -p "$results/$side"
        cp "$results/target-$side/release/examples/cell-bench" "$results/$side/cell-bench"
    done
    exec fxjob --bench --name cell-bench-ab -- env CELL_BENCH_AB_INNER=1 \
        "$here/cell-bench-ab.sh" "$a" "$b" "$results" "$rounds"
fi

# Inside the measurement: the stores' disk, ext4 over a loop device on tmpfs.
image=/dev/shm/cell-bench-ab-$$.img
mount=$results/disk
mkdir -p "$mount"
truncate -s 3G "$image"
mkfs.ext4 -q -F "$image"
sudo mount -o loop "$image" "$mount"
cleanup() {
    sudo umount "$mount" || true
    rm -f "$image"
}
trap cleanup EXIT
sudo chown "$(id -u):$(id -g)" "$mount"

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
