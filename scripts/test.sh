#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run clippy's rules (clippy.toml), the test suite, the simulation of the bucket's protocol, the concurrency
# tests under Shuttle and the models of memory orderings under loom, the scripts' tests
# (scripts/tests) and the mutant runners' checks, the Go client's (go/), and on Linux the guest
# tests' own tests (guest/tests, both halves) and the VMM's own that cladefs adds to
# (vendor/firecracker, built as cladefs builds it: scripts/firecracker-cargo.sh): what CI runs, a
# part a job.
# Arguments are passed to the suite's and the simulation's `cargo nextest run`. CLADEFS_TEST_PARTS
# names the parts to run, separated by spaces (default: all of clippy, workspace, sim, madsim,
# shuttle, scripts, go, and guest and firecracker on Linux). The part unit, in no default, runs the
# workspace's unit tests alone, about a minute: the workspace part's tests but the slow ones the
# nextest profile unit leaves out (.config/nextest.toml).
#
# Unicode tests compare against the Unicode Character Database, which is
# downloaded and checksummed on first use.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# glibc's fatal errors (a heap found corrupted, a stack smashed) on standard error, which the tests
# keep, rather than the terminal they have none of: an abort said nothing else (TASK-100).
export LIBC_FATAL_STDERR_=1

parts="clippy workspace sim madsim shuttle scripts go"
[[ $(uname) != Linux ]] || parts+=" guest firecracker"
parts=${CLADEFS_TEST_PARTS:-$parts}
part() { # name: whether CLADEFS_TEST_PARTS has it
    [[ " $parts " == *" $1 "* ]]
}
# Every part but the scripts' compiles, which a measurement (fxjob --bench) refuses to (TASK-368).
source "$root/scripts/lib/measurement.sh"
read -ra listed <<< "$parts"
if [[ ${listed[*]} != scripts ]]; then
    measurement_refuses "scripts/test.sh's parts ${listed[*]}"
fi

VERSION=12.1.0
SHA256=25ba51a0d4c6fa41047b7a5e5733068d4a734588f055f61e85f450097834a0a6
zip="$root/target/UCD-$VERSION.zip"

if [ ! -f "$zip" ]; then
    mkdir -p "$root/target"
    curl -fsSL -o "$zip" "https://www.unicode.org/Public/$VERSION/ucd/UCD.zip"
fi
echo "$SHA256  $zip" | shasum -a 256 -c - >/dev/null

# Clippy's rules the code keeps (clippy.toml): no blocking call in async code unless allowed, with
# why (TASK-44.20); and the compiler's warnings, as errors, the Shuttle builds' too (below), whose
# code no other build compiles: one went unread in a run's output, a drop of a usize (TASK-287).
# First, as it fails at once. Before them, rustfmt's (rustfmt.toml) on the workspace's crates: `cargo
# fmt` without --all, which would format the vendored trees their path dependencies reach too
# (vendor/firecracker, guest/rust/std), formatted as their upstreams keep them (TASK-436).
if part clippy; then
    # In $root: `cargo fmt --manifest-path` finds no targets in a virtual manifest.
    (cd "$root" && cargo fmt --check)
    UCD_ZIP="$zip" cargo clippy --manifest-path "$root/Cargo.toml" --workspace --tests -- \
        -A clippy::all -D clippy::disallowed_methods -D warnings
    cargo clippy --manifest-path "$root/Cargo.toml" -p local_cell -p physmem -p cladefs \
        -p follower -p follower_sim --features shuttle --tests -- -A clippy::all -D warnings
    cargo clippy --manifest-path "$root/Cargo.toml" -p physmem -p cladefs -p virtio_extent \
        --features loom --tests -- -A clippy::all -D warnings
fi
# The simulation of bucket_cell's protocol over local_cell's stores (TASK-88): its profiles'
# soaks, and its rules' seed searches, each until a seed with the rule off finds what the rule
# prevents, up to 20,000. Its own part, built with the profile sim, optimized with a debug build's
# checks: unoptimized, in the suite's build, four searches took 20 to 27 minutes each, which ran
# CI's workspace job out of time (TASK-188).
sim='package(local_cell) & test(/^sim::/)'
if part workspace; then
    UCD_ZIP="$zip" cargo nextest run --manifest-path "$root/Cargo.toml" --workspace \
        -E "not ($sim)" "$@"
fi
if part unit; then
    UCD_ZIP="$zip" cargo nextest run --manifest-path "$root/Cargo.toml" --workspace --profile unit "$@"
fi
if part sim; then
    cargo nextest run --manifest-path "$root/Cargo.toml" --cargo-profile sim -p local_cell \
        -E "$sim" "$@"
fi
# The build under madsim, the daemon's deterministic simulation (doc-24, TASK-685.12): the crates
# the simulation runs, built with `--cfg madsim`, its target a directory of its own, as every crate
# beneath them builds otherwise; and their tests under madsim, each crate's tests/madsim.rs. Their
# other tests are tokio's and std's, which madsim has no stand-ins for: they run in the workspace
# part.
if part madsim; then
    madsim=(env RUSTFLAGS="--cfg madsim" CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}/madsim")
    "${madsim[@]}" cargo check --manifest-path "$root/Cargo.toml" -p node_local -p bucket_cell \
        -p local_cell -p cladefs
    # Each test over MADSIM_TEST_NUM seeds, 20 unless said, from MADSIM_TEST_SEED's, if given.
    "${madsim[@]}" MADSIM_TEST_NUM="${MADSIM_TEST_NUM:-20}" cargo nextest run \
        --manifest-path "$root/Cargo.toml" -p node_local -p bucket_cell -p local_cell -p cladefs \
        --test madsim "$@"
fi
# The concurrency tests under Shuttle, built with each crate's `shuttle` feature, which only they
# can run (TASK-44).
if part shuttle; then
    cargo nextest run --manifest-path "$root/Cargo.toml" -p local_cell --features shuttle \
        --test shuttle
    cargo nextest run --manifest-path "$root/Cargo.toml" -p local_cell --features shuttle --lib \
        shuttle_tests::
    cargo nextest run --manifest-path "$root/Cargo.toml" -p physmem --features shuttle --lib \
        shuttle::
    cargo nextest run --manifest-path "$root/Cargo.toml" -p cladefs --features shuttle --lib \
        shuttle_tests::
    # The followers' concurrency protocol, under Shuttle (TASK-42.5).
    cargo nextest run --manifest-path "$root/Cargo.toml" -p follower --features shuttle
    # The followers' simulation: Fxfs as writer and followers on an extent store (TASK-42.1).
    cargo nextest run --manifest-path "$root/Cargo.toml" -p follower_sim --features shuttle
    # The protocols of atomics under loom, each crate's `loom` feature, whose every interleaving
    # and weak ordering Shuttle, seeing every atomic as SeqCst, can't run (docs/memory-ordering.md,
    # TASK-444.2).
    for crate in cladefs virtio_extent physmem; do
        cargo nextest run --manifest-path "$root/Cargo.toml" -p "$crate" --features loom --lib loom::
    done
fi
# The scripts' own tests, and that each hand mutant's patch or edits apply as they are: one that
# stopped applying would test nothing (guest/sim's need a kernel tree, so are CI's guest-sim job's,
# but for those of its model of Fxfs, which crates/fxfs-sim-diff runs).
if part scripts; then
    python3 -m unittest discover -s "$root/scripts/tests"
    # What the tests leave out says why, and a why naming a task names one open (TASK-149.20).
    "$root/scripts/check-exclusions.py"
    # Each source file carries the license header its origin needs.
    "$root/scripts/license-headers.py"
    "$root/crates/fxfs-posix/mutants.sh" --check
    "$root/crates/fxfs-sim-diff/mutants.sh" --check
    "$root/crates/cladefs/mutants.sh" --check
    "$root/crates/bucket_cell/mutants.sh" --check
    "$root/scripts/loom-mutants.sh" --check
    "$root/scripts/guest-mutants-host.py" --check
    # Firecracker's lock as Cargo resolves it with cladefs's crates, which only Linux builds: a
    # dependency added to them went unnoticed on macOS (TASK-395).
    git -C "$root" submodule update --init vendor/firecracker
    "$root/scripts/firecracker-cargo.sh" --check
fi
# The Go client of the daemon's control API (TASK-219): its generated code as the .proto generates
# it, its tests, and on Linux its tests against the daemon itself, built from this tree.
if part go; then
    "$root/go/generate.sh" --check
    unformatted=$(gofmt -l "$root/go")
    [[ -z $unformatted ]] || { echo "not gofmt'd: $unformatted" >&2; exit 1; }
    go -C "$root/go" vet -tags cladefsd ./...
    if [[ $(uname) == Linux ]]; then
        cargo build --manifest-path "$root/Cargo.toml" -p cladefs --bin cladefsd
        CLADEFSD="${CARGO_TARGET_DIR:-$root/target}/debug/cladefsd" \
            go -C "$root/go" test -tags cladefsd ./...
    else
        go -C "$root/go" test ./...
    fi
fi
# The guest tests' own tests, the host's half and the guest's, which build on Linux alone: their
# harness's (guest/tests/test_cases/src/vm.rs, script_vm.rs, the runner's) and the guest agent's.
# Those that mount a filesystem of their own on a loop device, with mkfs.ext4 and mkfs.xfs, need
# root, which nothing unprivileged stands in for (script_run's Loop): ignored, saying so, they
# run as root alone, as CI runs this part, and a run as another user says it leaves them out
# (TASK-307).
if part guest; then
    as_root=()
    if (( EUID == 0 )); then
        as_root=(--run-ignored all)
    else
        echo "scripts/test.sh: the guest part's tests that loop-mount a filesystem need root, and are left out: run it as root to include them" >&2
    fi
    harness=(cargo nextest run --manifest-path "$root/guest/tests/Cargo.toml"
        ${as_root[@]+"${as_root[@]}"})
    "${harness[@]}" -p test_cases --features host
    "${harness[@]}" -p test_cases --features guest
    "${harness[@]}" -p runner
fi
# The VMM's own tests that cladefs adds to, which no other suite reaches: its extent device's
# (TASK-79.102), its vCPUs' exits' (TASK-187) and the PCI transport's shared memory BAR's and device
# IDs' (TASK-267), on Linux alone, which the VMM builds for, with no KVM. With its cladefs feature, as
# cladefs builds it (scripts/firecracker-cargo.sh), and those its feature gates: the extent
# device's, its vCPUs' exits', its features pages' and its signal handlers'. Each extent test serves its store from a daemon in its process, a host's:
# its stores' cells in a bucket in memory, as of placement 1, which without
# CLADEFSD_BUCKET and CLADEFSD_TOKEN fails EIO opening it (TASK-220). userfaultfd-sys's
# bindgen needs libclang to build it.
if part firecracker; then
    tests='test(/extent::/) | test(/vcpu::tests::(efaults_|test_exit_code_)/)'
    tests+=' | test(/device::tests::test_features_/)'
    tests+=' | test(/signal_handler::/)'
    tests+=' | test(/transport::pci::device::tests::test_(shm_bar|pci_device_ids)$/)'
    CLADEFSD_BUCKET=memory:///vmm CLADEFSD_TOKEN=1 \
        "$root/scripts/firecracker-cargo.sh" nextest run -p vmm --lib --features cladefs -E "$tests"
    # The API's parsing of the device's configuration, PUT /cladefs/{id} (TASK-503).
    "$root/scripts/firecracker-cargo.sh" nextest run -p firecracker --bin firecracker \
        --features cladefs -E 'test(/request::cladefs::/)'
    # Where KVM is, as on ubu1: the PCI transport's tests and the device managers'
    # persistence tests, which make VMs, the extent device's window a BAR of its device's over PCI
    # (TASK-267); with CLADEFS_FIRECRACKER_TESTS=all, the vmm crate's whole suite, its integration
    # tests too, in their place. One at a time, as Firecracker runs them (--test-threads=1): they
    # share host resources, such as the tap device the persistence tests open by one name,
    # `hostname`: root's to make, or, made persistent and owned by a user (ip tuntap add dev
    # hostname mode tap user USER), that user's to open. Where neither, they are left out, saying
    # so (TASK-307).
    if [[ -w /dev/kvm ]]; then
        if [[ ${CLADEFS_FIRECRACKER_TESTS:-} == all ]]; then
            CLADEFSD_BUCKET=memory:///vmm CLADEFSD_TOKEN=1 \
                "$root/scripts/firecracker-cargo.sh" nextest run -p vmm --features cladefs \
                --no-fail-fast --test-threads 1
        else
            kvm_tests='(test(/transport::pci::/) | test(/device_manager::(pci_mngr|persist)::/))'
            tap=/sys/class/net/hostname/owner
            if (( EUID != 0 )) && ! [[ -r $tap && $(< "$tap") == "$EUID" ]]; then
                kvm_tests+=' - test(/::test_device_manager_persistence$/)'
                echo "scripts/test.sh: the VMM's persistence tests open the tap device hostname, which needs root or one of this user's (ip tuntap add dev hostname mode tap user $(id -un)), and are left out" >&2
            fi
            "$root/scripts/firecracker-cargo.sh" nextest run -p vmm --lib --features cladefs \
                --no-fail-fast --test-threads 1 -E "$kvm_tests"
        fi
    fi
fi
