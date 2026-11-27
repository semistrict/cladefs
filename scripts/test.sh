#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run clippy's rules (clippy.toml), the test suite, the simulation of the bucket's protocol, the concurrency
# tests under Shuttle and the models of memory orderings under loom, the scripts' tests
# (scripts/tests) and the mutant runners' checks, the Go client's (go/), and on Linux the guest
# tests' own tests (guest/tests, both halves) and the VMM's own that cladefs adds to
# (vendor/firecracker, built as cladefs builds it: scripts/firecracker-cargo.sh): what CI runs, a
# part a job; on an arm64 Mac, libmachine's, its Firecracker-compatible VMM's and machined's with
# their guests on Hypervisor.framework (the part hvf).
# Arguments are passed to the suite's and the simulation's `cargo nextest run`. CLADEFS_TEST_PARTS
# names the parts to run, separated by spaces (default: all of clippy, workspace, sim, madsim,
# shuttle, scripts, go, and guest and firecracker on Linux, hvf and guest on an arm64
# Mac). The part unit, in no default, runs the
# workspace's unit tests alone, about a minute: the workspace part's tests but the slow ones the
# nextest profile unit leaves out (.config/nextest.toml). The part worker, in no default, as it
# needs Node, pnpm and wasm-bindgen, builds the Cloudflare Worker collector as it is deployed and
# runs its checks and tests (workers/cladefs-gc).
#
# Every cargo call is --locked: a lock file that is not as Cargo resolves its manifests fails the
# part, rather than being rewritten under it (board 8601).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# glibc's fatal errors (a heap found corrupted, a stack smashed) on standard error, which the tests
# keep, rather than the terminal they have none of: an abort said nothing else (TASK-100).
export LIBC_FATAL_STDERR_=1

parts="clippy workspace sim madsim shuttle scripts go"
[[ $(uname) != Linux ]] || parts+=" guest firecracker"
[[ $(uname -sm) != "Darwin arm64" ]] || parts+=" hvf guest"
parts=${CLADEFS_TEST_PARTS:-$parts}
part() { # name: whether CLADEFS_TEST_PARTS has it
    [[ " $parts " == *" $1 "* ]]
}
# Every part but the scripts' compiles, which a measurement (hotbed run --bench) refuses to (TASK-368).
source "$root/scripts/lib/measurement.sh"
read -ra listed <<< "$parts"
if [[ ${listed[*]} != scripts ]]; then
    measurement_refuses "scripts/test.sh's parts ${listed[*]}"
fi

# The madsim build's RUSTFLAGS: RUSTFLAGS replaces .cargo/config.toml's target rustflags, so keep
# lld on Linux arm64 (x86_64-unknown-linux-gnu already defaults to rustc's bundled rust-lld).
madsim_rustflags="--cfg madsim"
if [[ $(uname -sm) == "Linux aarch64" ]]; then
    madsim_rustflags+=" -Clink-arg=-fuse-ld=lld"
fi

# Clippy's rules the code keeps (clippy.toml): no blocking call in async code unless allowed, with
# why (TASK-44.20); and the compiler's warnings, as errors, the Shuttle builds' too (below), whose
# code no other build compiles: one went unread in a run's output, a drop of a usize (TASK-287).
# First, as it fails at once. Before them, rustfmt's (rustfmt.toml) on the workspace's crates: `cargo
# fmt` without --all, which would format the vendored trees their path dependencies reach too
# (vendor/firecracker, guest/rust/std), formatted as their upstreams keep them (TASK-436).
if part clippy; then
    # In $root: `cargo fmt --manifest-path` finds no targets in a virtual manifest.
    (cd "$root" && cargo fmt --check)
    cargo clippy --locked --manifest-path "$root/Cargo.toml" --workspace --tests -- \
        -A clippy::all -D clippy::disallowed_methods -D warnings
    # cladefs's own crates (copied Fuchsia crates stay under `-A clippy::all` above): deny
    # clippy's correctness and suspicious lints too (board 8607), across tests and examples,
    # non-test lib and bin targets (without dev-dependencies unifying failpoints or testing:
    # boards 10218, 10489), and the Shuttle, loom and madsim builds below. `--no-deps` keeps
    # Fuchsia path dependencies under the workspace rule above.
    cladefs_crates=(
        -p base32_id -p block_device -p bucket_cell -p cladefs -p cladefs_core -p cladefs_fs
        -p cladefs_fuse -p cladefs_proto -p cladefs_wire -p cladefsd -p cladefs-examples
        -p extent_frame -p extent_keys -p formats -p fxfs-posix
        -p fxfs-sim-diff -p gc_wasm -p host_fs -p libcladefs -p local_cell -p node_local
        -p sqlite_cladefs -p vhost_user -p virtio_extent
    )
    cladefs_lints=(
        -A clippy::all -D clippy::correctness -D clippy::suspicious
        -D clippy::disallowed_methods -D warnings
    )
    cargo clippy --locked --manifest-path "$root/Cargo.toml" --no-deps \
        "${cladefs_crates[@]}" --tests --examples -- "${cladefs_lints[@]}"
    cargo clippy --locked --manifest-path "$root/Cargo.toml" --no-deps \
        "${cladefs_crates[@]}" --lib --bins -- "${cladefs_lints[@]}"
    # The daemon without its FUSE mount of cells (the `mount` feature off, board 14536), as a
    # process embedding it builds it (libcladefs), which no other build compiles.
    cargo clippy --locked --manifest-path "$root/Cargo.toml" --no-deps \
        -p cladefs -p cladefsd -p libcladefs --no-default-features --lib --bins -- "${cladefs_lints[@]}"
    cargo clippy --locked --manifest-path "$root/Cargo.toml" --no-deps \
        -p gc_wasm --target wasm32-unknown-unknown -- "${cladefs_lints[@]}"
    cargo clippy --locked --manifest-path "$root/Cargo.toml" --no-deps -p local_cell -p cladefs_core \
        --features shuttle --tests -- "${cladefs_lints[@]}"
    cargo clippy --locked --manifest-path "$root/Cargo.toml" --no-deps -p cladefs_core -p virtio_extent \
        -p vhost_user --features loom --tests -- "${cladefs_lints[@]}"
    # libmachine's crates (crates/libmachine*, doc-30) and machined's: all of clippy's default lints,
    # as libmachine kept them before it came into this repo; and its fuzz harness's build.
    libmachine_crates=(-p libmachine -p libmachine-agent -p libmachine-firecracker -p machined)
    cargo clippy --locked --manifest-path "$root/Cargo.toml" --no-deps "${libmachine_crates[@]}" \
        --all-targets -- -D warnings
    cargo clippy --locked --manifest-path "$root/Cargo.toml" --no-deps -p libmachine \
        --all-targets --features fuzzing -- -D warnings
    # The madsim build's (doc-24): the simulated crates and their tests under madsim, built as the
    # madsim part builds them, whose code no other build compiles either: its warnings, 13 on
    # Linux, went unseen there behind a `cargo check` (board 5652).
    env RUSTFLAGS="$madsim_rustflags" CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}/madsim" \
        cargo clippy --locked --manifest-path "$root/Cargo.toml" --no-deps -p node_local -p host_fs \
        -p bucket_cell -p local_cell -p cladefs_core -p cladefs_fs -p cladefs_fuse -p cladefs_wire -p cladefsd -p cladefs -p libcladefs --lib --test madsim -- "${cladefs_lints[@]}"
fi
# The simulation of bucket_cell's protocol over local_cell's extent stores (TASK-88): its profiles'
# soaks, and its rules' seed searches, each until a seed with the rule off finds what the rule
# prevents, up to 20,000. Its own part, built with the profile sim, optimized with a debug build's
# checks: unoptimized, in the suite's build, four searches took 20 to 27 minutes each, which ran
# CI's workspace job out of time (TASK-188).
sim='package(local_cell) & test(/^sim::/)'
if part workspace; then
    cargo nextest run --locked --manifest-path "$root/Cargo.toml" --workspace \
        -E "not ($sim)" "$@"
fi
if part unit; then
    cargo nextest run --locked --manifest-path "$root/Cargo.toml" --workspace \
        --profile unit "$@"
fi
if part sim; then
    cargo nextest run --locked --manifest-path "$root/Cargo.toml" --cargo-profile sim \
        -p local_cell -E "$sim" "$@"
fi
# The Cloudflare Worker collector (workers/cladefs-gc, board 14524): gc_wasm built for wasm32 as
# it is deployed (its scripts/wasm.sh, wasm-bindgen of Cargo.lock's version), then the Worker
# installed by its lockfile, type-checked, linted and formatted (`check`), its tests run against
# Miniflare's R2 and its wasm collector (`test`), and bundled (`build`). What runs the collector on
# wasm: the clippy part lints gc_wasm for wasm32, and runs none of it.
if part worker; then
    "$root/workers/cladefs-gc/scripts/wasm.sh"
    (cd "$root/workers/cladefs-gc" && pnpm install --frozen-lockfile && pnpm run check &&
        pnpm test --run && pnpm run build)
fi
# The build under madsim, the daemon's deterministic simulation (doc-24, TASK-685.12): the crates
# the simulation runs, built with `--cfg madsim`, its target a directory of its own, as every crate
# beneath them builds otherwise; and their tests under madsim, each crate's tests/madsim.rs. Their
# other tests are tokio's and std's, which madsim has no stand-ins for: they run in the workspace
# part.
if part madsim; then
    madsim=(env RUSTFLAGS="$madsim_rustflags" CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}/madsim")
    "${madsim[@]}" cargo check --locked --manifest-path "$root/Cargo.toml" -p node_local \
        -p host_fs -p bucket_cell -p local_cell -p cladefs_core -p cladefs_fs -p cladefs_fuse -p cladefs_wire -p cladefsd -p cladefs
    # Each test over CLADEFS_MADSIM_SEEDS seeds, 20 unless said, from MADSIM_TEST_SEED's, 1 unless
    # said, after the regression seeds, those that once failed (crates/cladefs/tests/madsim-seeds,
    # TASK-685.8): each seed a process of its own, as what a simulation's hosts held in memory,
    # leaked as their node-local values are, stays in its process (their physical memory's mappings
    # and descriptors let go as each ends, board 2890). Every seed runs; failures of open bugs
    # (crates/cladefs/tests/madsim-known) are said as known, and any other fails the part
    # (scripts/madsim-seeds.py, board 2912). On macOS as on Linux.
    "${madsim[@]}" "$root/scripts/madsim-seeds.py" --first "${MADSIM_TEST_SEED:-1}" \
        --count "${CLADEFS_MADSIM_SEEDS:-20}" "$@"
    # A rule search, `*_needs_*`, runs its own seeds, once.
    "${madsim[@]}" cargo nextest run --locked --manifest-path "$root/Cargo.toml" -p node_local \
        -p host_fs -p bucket_cell -p local_cell -p cladefs --test madsim -E 'test(/_needs_/)' \
        --no-tests=pass "$@"
fi
# The concurrency tests under Shuttle, built with each crate's `shuttle` feature, which only they
# can run (TASK-44).
if part shuttle; then
    cargo nextest run --locked --manifest-path "$root/Cargo.toml" -p local_cell --features shuttle \
        --test shuttle
    cargo nextest run --locked --manifest-path "$root/Cargo.toml" -p local_cell --features shuttle \
        --lib shuttle_tests::
    cargo nextest run --locked --manifest-path "$root/Cargo.toml" -p cladefs_core --features shuttle \
        --lib shuttle_tests::
    # The protocols of atomics under loom, each crate's `loom` feature, whose every interleaving
    # and weak ordering Shuttle, seeing every atomic as SeqCst, can't run (docs/memory-ordering.md,
    # TASK-444.2).
    for crate in cladefs_core virtio_extent vhost_user; do
        cargo nextest run --locked --manifest-path "$root/Cargo.toml" -p "$crate" --features loom \
            --lib loom::
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
        source "$root/scripts/lib/musl.sh"
        musl_setup
        env "${MUSL_ENV[@]}" cargo build --locked --manifest-path "$root/Cargo.toml" \
            --target "$MUSL_TARGET" -p cladefs --bin cladefsd
        # No headroom, as cargo's tests' daemons have (.cargo/config.toml): a cell refused only
        # once the host's memory is really full (board 5662).
        CLADEFSD="${CARGO_TARGET_DIR:-$root/target}/$MUSL_TARGET/debug/cladefsd" \
            CLADEFSD_HEADROOM=${CLADEFSD_HEADROOM:-0} go -C "$root/go" test -tags cladefsd ./...
    else
        go -C "$root/go" test ./...
    fi
fi
# The guest tests' own tests, the host's half and the guest's: their harness's
# (guest/tests/test_cases/src/vm.rs, script_vm.rs, the runner's) and the guest agent's, the
# guest's half Linux's code, on Linux alone; the host's on an arm64 Mac too, which runs the guests
# on libmachine (board 14336). Those that mount a filesystem of their own on a loop device, with
# mkfs.ext4 and mkfs.xfs, need root, which nothing unprivileged stands in for (script_run's Loop):
# ignored, saying so, they run as root alone, as CI runs this part, and a run as another user says
# it leaves them out (TASK-307).
if part guest; then
    as_root=()
    if (( EUID == 0 )); then
        as_root=(--run-ignored all)
    elif [[ $(uname) == Linux ]]; then
        echo "scripts/test.sh: the guest part's tests that loop-mount a filesystem need root, and are left out: run it as root to include them" >&2
    fi
    harness=(cargo nextest run --locked --manifest-path "$root/guest/tests/Cargo.toml"
        ${as_root[@]+"${as_root[@]}"})
    "${harness[@]}" -p test_cases --features host
    if [[ $(uname) == Linux ]]; then
        "${harness[@]}" -p test_cases --features guest
    fi
    "${harness[@]}" -p runner
fi
# On an arm64 Mac, libmachine's, libmachine-firecracker's and machined's tests on
# Hypervisor.framework (board 13063), with
# cladefs's arm64 kernel and the test initramfs, which are Linux's to build:
# crates/libmachine/scripts/arm64-guest.sh builds them on hotbed and fetches them into
# target/arm64-guest. Niced, as the Mac is the user's (AGENTS.md: the one local build). A test that
# skipped itself, its guest missing, fails the part.
if part hvf; then
    arm64_guest=${LIBMACHINE_GUEST_DIR:-$root/target/arm64-guest}
    [[ -f $arm64_guest/initramfs.cpio && -f ${CLADEFS_GUEST_KERNEL:-$arm64_guest/Image} ]] ||
        { echo "scripts/test.sh: no arm64 guest in $arm64_guest: run crates/libmachine/scripts/arm64-guest.sh" >&2; exit 1; }
    hvf_log=$(mktemp)
    nice -n 19 cargo nextest run --locked --manifest-path "$root/Cargo.toml" \
        -p libmachine -p libmachine-agent -p libmachine-firecracker -p machined \
        --success-output final "$@" 2>&1 | tee "$hvf_log"
    if grep -q "skipping:" "$hvf_log"; then
        grep "skipping:" "$hvf_log" | sort | uniq -c >&2
        echo "scripts/test.sh: libmachine's or machined's tests above skipped themselves" >&2
        exit 1
    fi
    rm "$hvf_log"
fi
# The VMM's own tests that cladefs adds to, which no other suite reaches: its extent device's
# (TASK-79.102), its vCPUs' exits' (TASK-187) and the PCI transport's shared memory BAR's and device
# IDs' (TASK-267), on Linux alone, which the VMM builds for, with no KVM. With its cladefs feature, as
# cladefs builds it (scripts/firecracker-cargo.sh), and those its feature gates: the extent
# device's, its vCPUs' exits', its features pages' and its signal handlers'. Each extent test serves its cell from a daemon in its process, a host's:
# its cells in a bucket in memory, as of placement 1, which without
# CLADEFSD_BUCKET and CLADEFSD_TOKEN fails EIO opening it (TASK-220). userfaultfd-sys's
# bindgen needs libclang to build it.
if part firecracker; then
    tests='test(/extent::/) | test(/vcpu::tests::(efaults_|test_exit_code_)/)'
    tests+=' | test(/device::tests::test_features_/)'
    tests+=' | test(/block::vhost_user::device::tests::/)'
    tests+=' | test(/transport::mmio::tests::test_ack_features_by_page$/)'
    tests+=' | test(/vsock::device::tests::(test_virtio_device|test_prepare_save_|test_kick_signals_)/)'
    tests+=' | test(/vsock::csm::connection::tests::/) | test(/vsock::unix::muxer::tests::/)'
    tests+=' | test(/signal_handler::/) | test(/cladefs_memory::/)'
    tests+=' | test(/transport::pci::device::tests::test_(shm_bar|pci_device_ids)$/)'
    CLADEFSD_BUCKET=memory:///vmm CLADEFSD_TOKEN=1 \
        "$root/scripts/firecracker-cargo.sh" nextest run -p vmm --lib --features cladefs -E "$tests"
    # The jailer's /proc/misc parsing tests (no KVM or root needed).
    "$root/scripts/firecracker-cargo.sh" nextest run -p jailer --bin jailer \
        -E 'test(/test_(parse_)?misc_dev_minor/)'
    # The API's parsing of the device's configuration, PUT /cladefs/{id} (TASK-503).
    "$root/scripts/firecracker-cargo.sh" nextest run -p firecracker --bin firecracker \
        --features cladefs -E 'test(/request::cladefs::/)'
    # Where KVM is, as on the test host: the PCI transport's tests and the device managers'
    # persistence tests, which make VMs, the extent device's window a BAR of its device's over PCI
    # (TASK-267); with CLADEFS_FIRECRACKER_TESTS=all, the vmm crate's whole suite, its integration
    # tests too, in their place (which boot with no memory cell, and fail, since board 14415: board
    # 14513). One at a time, as Firecracker runs them (--test-threads=1): they
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
