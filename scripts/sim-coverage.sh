#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# cladefs: the branch coverage of the simulations alone (board 2842, TASK-685.10): which branches
# of the protocol's code no seed reaches. Runs a simulation under coverage (cargo-llvm-cov, over
# rustc's -C instrument-coverage, its branches nightly's), none of the unit tests, and writes to
# OUT (default target/sim-coverage/WHICH):
#
#   coverage.json   llvm-cov's export, every file's regions, lines and branches
#   branches.md     the protocol files' branch coverage, and each branch side no seed took
#                   (scripts/coverage-branches.py)
#
#   scripts/sim-coverage.sh local [OUT]    local_cell::sim's profiles: the storage layer's
#                                          simulation, its rule searches aside (macOS or Linux)
#   scripts/sim-coverage.sh madsim [OUT]   the full-stack simulation (crates/cladefs/tests/
#                                          madsim.rs), its rule searches aside, over
#                                          CLADEFS_MADSIM_SEEDS seeds (20 unless said) from
#                                          MADSIM_TEST_SEED's (1 unless said), each a process of
#                                          its own as scripts/test.sh runs them (Linux, as its
#                                          daemon is)
#
# Needs a nightly toolchain with llvm-tools, guest/rust's dated one unless CLADEFS_COVERAGE_TOOLCHAIN
# names another (`rustup toolchain install nightly-YYYY-MM-DD --component llvm-tools`; the one
# nightly the repository pins, board 8603), and cargo-llvm-cov (MIT/Apache-2.0).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
which=${1:?usage: scripts/sim-coverage.sh local|madsim [OUT]}
out=${2:-$root/target/sim-coverage/$which}
toolchain=${CLADEFS_COVERAGE_TOOLCHAIN:-$(sed -n 's/^channel = "\(.*\)"$/\1/p' \
    "$root/guest/rust/rust-toolchain.toml")}
mkdir -p "$out"

# The protocol's code: the stores' uploads and cells, the bucket's writer and heads, and the
# daemon's cells, requests and peers.
protocol=(
    local_cell/src/upload.rs local_cell/src/cell.rs
    bucket_cell/src/writer.rs bucket_cell/src/head.rs
    cladefs/src/cladefsd/cells.rs cladefs/src/cladefsd/server.rs cladefs/src/cladefsd/peers.rs
    cladefs/src/cladefsd/migration.rs cladefs/src/host_cell.rs cladefs/src/open_cell.rs
)

cov=(cargo "+$toolchain" llvm-cov)
case "$which" in
local)
    # Its profiles, test_holds! and the like; not its searches (test_needs!, test_leaks!), which
    # run with a rule off, reaching what the protocol never does.
    profiles='test(/^sim::/) & not test(/_needs?_/) & not test(=sim::origins_are_kept_across_regions)'
    profiles+=' & not test(=sim::references_hold_across_regions)'
    export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}/coverage
    "${cov[@]}" clean --workspace
    "${cov[@]}" nextest --manifest-path "$root/Cargo.toml" --branch --cargo-profile sim \
        -p local_cell --no-report -E "$profiles"
    "${cov[@]}" report --manifest-path "$root/Cargo.toml" --branch --profile sim --json \
        --output-path "$out/coverage.json"
    ;;
madsim)
    export RUSTFLAGS="--cfg madsim" CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}/madsim-coverage
    "${cov[@]}" clean --workspace
    first=${MADSIM_TEST_SEED:-1}
    for seed in $(seq "$first" $((first + ${CLADEFS_MADSIM_SEEDS:-20} - 1))); do
        # A seed failing is the simulation's finding, not the coverage's: its run counts still.
        MADSIM_TEST_SEED=$seed "${cov[@]}" nextest --manifest-path "$root/Cargo.toml" --branch \
            -p cladefs --features testing --test madsim --no-report -E 'not test(/_needs_/)' \
            || echo "sim-coverage: seed $seed failed: MADSIM_TEST_SEED=$seed replays it" >&2
    done
    "${cov[@]}" report --manifest-path "$root/Cargo.toml" --branch --json \
        --output-path "$out/coverage.json"
    ;;
*)
    echo "usage: scripts/sim-coverage.sh local|madsim [OUT]" >&2
    exit 2
    ;;
esac
"$root/scripts/coverage-branches.py" "$out/coverage.json" "${protocol[@]}" > "$out/branches.md"
echo "sim-coverage: $out/branches.md"
