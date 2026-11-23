# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# What the scripts that run cladefs on an x86-64 or arm64 Linux KVM host, as
# root (scripts/lib/*-linux-host.sh), share, sourced by them: provisioning the
# host (host_setup), the tree each builds in (host_tree), the tools it needs,
# at the versions pinned here (host_need), the guest kernel it builds
# (host_kernel), and the record of the host it ran on (host_record). Sourcing it
# defines them and does nothing else. The sourcing script sets repo, the source,
# and results, the directory it writes to. The Lima VM's tree is a host_tree
# too, in the VM's home (scripts/lib/lima.sh).
#
# A test host, provisioned already (by its own setup),
# sets CLADEFS_TEST_HOST_PROVISIONED=1: host_setup checks it and installs nothing, the
# caller's Rust (PATH, CARGO_HOME, RUSTUP_HOME) is what builds, and host_need
# takes a tool there at whatever version it is. CLADEFS_TEST_HOST_WORK names the work
# directory, where the trees, guest kernels and artifacts go (default
# /var/lib/fxdax-test), CLADEFS_TEST_HOST_KERNELS a cache of guest kernels that
# outlasts it (host_kernel), and CLADEFS_TEST_HOST_IN_PLACE=1 has each tree build where
# it is (host_tree): scripts/lib/host.sh's runs set all four.
#
# A measurement's run is two jobs of the same work directory (scripts/lib/host.sh's
# host_job), each a run of the host script, which CLADEFS_TEST_HOST_PHASE tells apart,
# host_phase here: `build`, as work, its builds alone, into the work directory; then
# `measure`, alone, under hotbed run --bench, against what the build made there, building
# nothing (host_artifacts, host_prebuilt), which scripts/lib/measurement.sh's guard
# refuses there (host_cargo, host_kernel). Unset, `all`: one job does both.

work=${CLADEFS_TEST_HOST_WORK:-/var/lib/fxdax-test}
# The tools host_need installed, which host_record records.
host_needed=()
# This tree, whose scripts build what the host scripts measure.
host_tools=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
source "$host_tools/scripts/lib/measurement.sh"
# The host's packages (host_packages_install) and apt-get's runs (host_apt).
source "$host_tools/scripts/lib/host-packages.sh"

# host_setup provisions the host, once per host for what lasts: the packages,
# the Rust toolchain and its musl target; and exports the environment the
# builds and tests run in. It makes results absolute, and sets host_phase.
host_setup() {
    host_phase=${CLADEFS_TEST_HOST_PHASE:-all}
    [[ $host_phase == all || $host_phase == build || $host_phase == measure ]] ||
        { echo "CLADEFS_TEST_HOST_PHASE is build or measure, or unset for both" >&2; return 2; }
    mkdir -p "$work/tools" "$results"
    results=$(cd "$results" && pwd)
    case $(uname -m) in
        x86_64 | aarch64) ;;
        *) echo "host_setup: cladefs's hosts are x86-64 or arm64, not $(uname -m)" >&2; return 1 ;;
    esac
    test -c /dev/kvm

    host_network
    if [[ ${CLADEFS_TEST_HOST_PROVISIONED:-} == 1 ]]; then
        command -v cargo > /dev/null ||
            { echo "CLADEFS_TEST_HOST_PROVISIONED: no cargo on PATH" >&2; return 1; }
        return 0
    fi
    export DEBIAN_FRONTEND=noninteractive
    # scripts/host-packages.txt's, once per list.
    host_packages_install "$work/tools"

    RUSTUP_TOOLCHAIN=$(pinned_version rust)
    export CARGO_HOME="$work/tools/cargo" RUSTUP_HOME="$work/tools/rustup" RUSTUP_TOOLCHAIN
    export PATH="$CARGO_HOME/bin:$PATH"
    host_rust

    # The guest tests' runner gives each case a network of its own (TASK-149.20.6):
    # as root, as here, with `unshare --net`, not `buildah unshare`, which failed
    # as root (memfd_create: invalid argument) and had the cases run unisolated.
}

# host_rust installs rustup, in CARGO_HOME and RUSTUP_HOME, and the toolchain the
# hosts build with, RUSTUP_TOOLCHAIN, with its musl target and clippy, unless they
# are there and work. A toolchain whose install was cut off, the job killed with
# its SSH session, has no manifest, and every rustup and cargo after fails
# ("missing manifest in toolchain", GCE r980): one whose rustc doesn't run is
# removed, with the update hash naming it, and installed anew.
host_rust() {
    if [[ ! -x "$CARGO_HOME/bin/cargo" ]]; then
        curl -fsSL --retry 5 --connect-timeout 60 --speed-limit 1024 --speed-time 60 \
            https://sh.rustup.rs -o "$work/tools/rustup-init.sh"
        sh "$work/tools/rustup-init.sh" -y --profile minimal --default-toolchain "$RUSTUP_TOOLCHAIN" \
            --no-modify-path
    fi
    if ! rustc --version > /dev/null 2>&1; then
        local part
        for part in "$RUSTUP_HOME/toolchains/$RUSTUP_TOOLCHAIN"-* \
            "$RUSTUP_HOME/update-hashes/$RUSTUP_TOOLCHAIN"-*; do
            if [[ -e $part ]]; then rm -r -- "$part"; fi
        done
        rustup toolchain install --no-self-update "$RUSTUP_TOOLCHAIN" --profile minimal
    fi
    rustup target add "$(uname -m)-unknown-linux-musl"
    # scripts/test.sh checks clippy's rules first (TASK-44.20).
    rustup component add clippy
}

# pinned_version TOOL prints the version of TOOL the hosts run, its pinned version: Rust's, the
# one rust-toolchain.toml pins for CI and every build of the workspace (board 8603), or that of a
# tool cargo installs (host_need).
pinned_version() {
    case $1 in
        rust) sed -n 's/^channel = "\(.*\)"$/\1/p' "$host_tools/rust-toolchain.toml" ;;
        cargo-nextest) echo 0.9.143 ;;
        cargo-mutants) echo 27.1.0 ;;
        *) echo "pinned_version: $1 has no version pinned" >&2; return 1 ;;
    esac
}

# host_version TOOL prints the version of TOOL installed, as the tool says it.
host_version() {
    case $1 in
        perf) perf --version ;;
        # Its first line, as cargo-nextest says more.
        cargo-*) cargo "${1#cargo-}" --version | sed -n 1p ;;
        *) echo "host_version: $1 is no tool host_need installs" >&2; return 1 ;;
    esac
}

# host_network bounds what cargo fetches for a run: a fetch receiving less
# than a KiB a second for a minute fails, tried again a few times, and then
# fails the run, whose job says so in its status, rather than hang it without
# end; HTTP/2 multiplexing off, so that each fetch is a connection of its own.
# GCE r940 waited 2 h 40 min at "Updating crates.io index", its load 0, its
# connection dropped as the host's services restarted, as r990's did (2026-11-08).
# What the caller set stays.
host_network() {
    export CARGO_HTTP_TIMEOUT=${CARGO_HTTP_TIMEOUT:-60}
    export CARGO_HTTP_LOW_SPEED_LIMIT=${CARGO_HTTP_LOW_SPEED_LIMIT:-1024}
    export CARGO_NET_RETRY=${CARGO_NET_RETRY:-5}
    export CARGO_HTTP_MULTIPLEXING=${CARGO_HTTP_MULTIPLEXING:-false}
}

# host_need TOOL... installs each TOOL, unless it is there: perf, which follows
# the running kernel and so has no pinned version, or a cargo subcommand, at its
# pinned version (pinned_version). host_record records them.
host_need() {
    local tool pinned
    for tool in "$@"; do
        case $tool in
            perf)
                if ! command -v perf > /dev/null; then
                    host_apt install -y -qq linux-tools-common "linux-tools-$(uname -r)" \
                        > /dev/null
                fi
                ;;
            bpftrace)
                command -v bpftrace > /dev/null ||
                    host_apt install -y -qq bpftrace > /dev/null
                ;;
            *)
                pinned=$(pinned_version "$tool")
                if [[ ${CLADEFS_TEST_HOST_PROVISIONED:-} == 1 ]] &&
                    host_version "$tool" > /dev/null 2>&1; then
                    :
                elif [[ "$(host_version "$tool" 2> /dev/null)" != "$tool $pinned" ]]; then
                    cargo install "$tool" --version "$pinned" --locked
                fi
                ;;
        esac
        host_needed+=("$tool")
    done
}

# host_tree NAME copies the source to the tree NAME, which persists between
# runs so that a run builds only what changed, and versions repo at it. By
# content, not times, which a tree unpacked from another checkout's has: only
# what differs is written, which makes it newer than what was built from it, as
# cargo and make need. That holds only against what the tree itself built, so
# each tree builds in target directories of its own, which the copy keeps, as it
# keeps Firecracker's build: in none that another tree builds in too. Git's
# files, which a checkout has, are not the tree's.
#
# A tree copied by times, as with rsync -a, may hold content newer than its
# times, beside what was built from what it replaced: so a copy by content
# writes every file again unless the tree's last copy was by content too, which
# .by-content in the tree records. A copy into the tree by other means that
# deletes what the source lacks, as the scripts' own before (rsync -a --delete),
# deletes it.
#
# With CLADEFS_TEST_HOST_IN_PLACE=1, the source is a tree of the run's own, a worktree
# whose target directories are its own and go with it (scripts/lib/host.sh's
# runs): it builds in place, keeping its .git, which the scripts' tests read
# (git ls-files), and repo stays.
host_tree() {
    if [[ ${CLADEFS_TEST_HOST_IN_PLACE:-} == 1 ]]; then return 0; fi
    local tree="$work/$1" compare=--checksum
    mkdir -p "$tree"
    if [[ ! -e $tree/.by-content ]]; then compare=--ignore-times; fi
    rsync -rlpD "$compare" --delete --exclude target --exclude .git \
        --exclude /vendor/firecracker/build --exclude /.by-content "$repo/" "$tree/"
    touch "$tree/.by-content"
    repo=$tree
}

# host_kernel KIND [WORK] builds cladefs's x86-64 guest kernel of KIND from the source
# (scripts/build-guest-kernel.sh): release; debug (lockdep, KCSAN, DEBUG_ATOMIC_SLEEP); kasan
# (KASAN, KFENCE, lockdep); coverage (gcov of fs/cladefs); or rust-checks (its Rust's debug
# assertions and overflow checks). It builds in the work directory WORK (default
# $work/guest-kernel-KIND), its log host_kernel_log (default RESULTS/guest-kernel.log), and
# refuses to inside a measurement (measurement.sh), and versions host_kernel_image at the
# kernel and host_kernel_work at where it was built. The kernel's Rust builds on guest/rust's own
# toolchain, not the host's. On an x86-64 host alone: the guests' kernel is x86-64's (a driver
# whose run boots guests refuses another host first, scripts/lib/host.sh's host_need_arch).
#
# With CLADEFS_TEST_HOST_KERNELS naming a cache of kernels, as a hotbed workspace's runs do, whose
# work directories come and go (scripts/lib/host.sh: the checkout's target/guest-kernels, which
# scripts/guest-kernel.sh fills too), it takes the kernel of KIND built from the same sources
# (host_kernel_key) from there, if one was; else it builds it in the cache's work directory of
# KIND, which keeps the last kernel's build, so that only what changed since builds, and keeps it,
# with the last few others. One build of a KIND at a time. A coverage kernel builds in WORK alone,
# as its counts are read against its build's notes (scripts/guest-coverage.py).
host_kernel() {
    local kind=$1 kernel_work=${2:-$work/guest-kernel-$1} channel key cache built old
    local log=${host_kernel_log:-$results/guest-kernel.log}
    local debug=0 kasan=0 coverage=0 rust_checks=0
    case $kind in
        release) ;;
        debug) debug=1 ;;
        kasan) kasan=1 ;;
        coverage) coverage=1 ;;
        rust-checks) rust_checks=1 ;;
        *) echo "host_kernel: a kernel is release, debug, kasan, coverage or rust-checks" >&2; return 2 ;;
    esac
    if [[ $(uname -m) != x86_64 ]]; then
        echo "host_kernel: the guest kernel is x86-64's, and this host is $(uname -m)" >&2
        return 2
    fi
    channel=$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$repo/guest/rust/rust-toolchain.toml")
    rustup toolchain install --no-self-update "$channel" --component rust-src --profile minimal > /dev/null || return
    if [[ -z ${CLADEFS_TEST_HOST_KERNELS:-} || $kind == coverage ]]; then
        host_kernel_build "$kernel_work" || return
        # shellcheck disable=SC2034 # The sourcing scripts read them.
        host_kernel_work=$kernel_work host_kernel_image=$kernel_work/out/x86_64/vmlinux
        return 0
    fi
    key=$(host_kernel_key "$kind") || return
    cache=$CLADEFS_TEST_HOST_KERNELS/$kind
    built=$cache/built/$key/vmlinux
    mkdir -p "$cache/built" || return
    exec 7> "$cache.lock"
    flock 7 || return
    if [[ -f $built ]]; then
        echo "The $kind guest kernel of these sources, $key, built before: $built" > "$log"
    else
        host_kernel_build "$cache/work" || return
        mkdir -p "$(dirname "$built")" || return
        cp "$cache/work/out/x86_64/vmlinux" "$built.new" && mv "$built.new" "$built" || return
        # The last few, each a kernel's image alone.
        while read -r old; do
            rm -r -- "${cache:?}/built/$old" || return
        done < <(ls -1t "$cache/built" | sed -n '5,$p')
    fi
    exec 7>&-
    # shellcheck disable=SC2034 # The sourcing scripts read them.
    host_kernel_work=$cache/work host_kernel_image=$built
}

# host_kernel_build WORK builds host_kernel's kernel in the work directory WORK.
host_kernel_build() {
    measurement_refuses "The $kind guest kernel's build"
    env -u RUSTUP_TOOLCHAIN CLADEFS_GUEST_KERNEL_WORK="$1" \
        CLADEFS_GUEST_KERNEL_DEBUG="$debug" CLADEFS_GUEST_KERNEL_KASAN="$kasan" \
        CLADEFS_GUEST_KERNEL_COVERAGE="$coverage" CLADEFS_GUEST_KERNEL_RUST_CHECKS="$rust_checks" \
        "$repo/scripts/build-guest-kernel.sh" x86_64 > "$log" 2>&1
}

# host_cargo ARG... runs cargo ARG..., a build, which a measurement refuses (measurement.sh): a
# measurement's builds run in its build phase (host_phase), as work.
host_cargo() {
    measurement_refuses "cargo $1"
    cargo "$@"
}

# host_artifacts DIR NAME [KERNEL] builds what the guest cases run of repo into DIR, test-e2e.sh's
# artifacts (CLADEFS_E2E_BUILD_ONLY), by this tree's scripts (CLADEFS_E2E_ROOT: repo may be of
# before them), and, given KERNEL, a guest kernel, copies it there as vmlinux, recorded with them
# in DIR/artifacts.txt; RESULTS/build-NAME.log says how the build went, and
# RESULTS/artifacts-NAME.txt what it made. A measurement runs them (host_prebuilt), building
# nothing.
host_artifacts() {
    local dir=$1 name=$2 kernel=${3:-}
    if [[ -e $dir ]]; then rm -r -- "$dir" || return; fi
    if ! CLADEFS_E2E_ROOT=$repo CLADEFS_E2E_BUILD_ONLY=$dir CLADEFS_E2E_RESULTS=$results/build-$name \
        "$host_tools/scripts/test-e2e.sh" > "$results/build-$name.log" 2>&1; then
        echo "Building $name's artifacts failed: see $results/build-$name.log" >&2
        return 1
    fi
    if [[ -n $kernel ]]; then
        cp "$kernel" "$dir/vmlinux" || return
        (cd "$dir" && sha256sum vmlinux) >> "$dir/artifacts.txt" || return
    fi
    cp "$dir/artifacts.txt" "$results/artifacts-$name.txt"
}

# host_prebuilt DIR has the guest cases run what host_artifacts built into DIR, building nothing:
# test-e2e.sh's CLADEFS_E2E_PREBUILT, and its guest kernel, if it holds one.
host_prebuilt() {
    [[ -f $1/artifacts.txt ]] ||
        { echo "$1 holds no artifacts: CLADEFS_TEST_HOST_PHASE=build makes them" >&2; return 2; }
    export CLADEFS_E2E_PREBUILT=$1
    if [[ -f $1/vmlinux ]]; then export CLADEFS_GUEST_KERNEL=$1/vmlinux; fi
}

# host_kernel_key KIND prints the key of the guest kernel of KIND built from the source: KIND, and
# the key of what builds it (scripts/guest-kernel-key.sh, this tree's, as the source may be of a
# commit before it).
host_kernel_key() {
    local sources
    sources=$("$(dirname "${BASH_SOURCE[0]}")/../guest-kernel-key.sh" "$repo") || return
    echo "$1-${sources:0:16}"
}

# host_record prints what the run ran on: the host, its CPU and memory, Rust,
# and the tools host_need installed.
host_record() {
    local tool
    uname -a
    # The CPU: x86-64's model name; an arm64 one's implementer and part, which its /proc/cpuinfo
    # has in its place (a run on an arm64 test host ended at this grep, TASK-722).
    grep -m1 'model name' /proc/cpuinfo ||
        echo "cpu $(uname -m) $(grep -m2 -E '^CPU (implementer|part)' /proc/cpuinfo |
            sed 's/[[:space:]]*:[[:space:]]*/ /' | paste -sd ' ' -)"
    echo "cpus $(nproc)"
    grep MemTotal /proc/meminfo
    rustc --version
    for tool in ${host_needed[@]+"${host_needed[@]}"}; do
        host_version "$tool"
    done
}
