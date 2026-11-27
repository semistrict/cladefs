#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run cladefs's guest tests (guest/tests) on this host, Linux with KVM, or an arm64 Mac (below;
# elsewhere, use scripts/test-host.sh, which runs them on a test host). Arguments select the test cases, by name or
# by suite, `--tag SUITE` (the guest tests' runner's `list`, whose --help names the suites;
# default: --tag e2e), which boot cladefs's Firecracker (vendor/firecracker,
# with its cladefs feature), which this builds, and the guest kernel
# CLADEFS_GUEST_KERNEL names, or, unset, this tree's (scripts/guest-kernel.sh, board 7427). Each
# test's logs are kept under $CLADEFS_E2E_RESULTS (default: a new temporary directory).
# CLADEFS_E2E_REPEATS runs each case that many times (default 1), as a soak, each a
# repeat: it stops at a case's first failing repeat, whose logs it keeps, in repeat-K
# under the case's, and drops the passing repeats' but for what their guests said, in
# repeat-K.log there, which measurements taken repeat by repeat need (pgbench's tps).
# Each repeat's outcome is kept as data, in repeat-K.json there, and the run's in summary.json
# and run.json beside the cases' (scripts/guest_results.py, which says what each holds).
# Firecracker runs under its production seccomp filter (CLADEFS_FIRECRACKER_SECCOMP, real by
# default; log, or none), and CLADEFS_FIRECRACKER_JAIL=1 in its jailer's jail, as an unprivileged
# user (see below).
#
# cladefs: CLADEFS_E2E_BUILD_ONLY=DIR builds what the cases run, and copies it into DIR, as its
# artifacts: the runner, the guest agent, the extent daemon, cladefsctl, cladefs-gc and Firecracker;
# it runs no test, and, under a seccomp filter, Firecracker's jailer. CLADEFS_E2E_PREBUILT=DIR runs
# the cases with DIR's artifacts, building nothing, nor running the VMM's tests first: so that an
# A/B of two trees (scripts/bench-ab-host.sh) alternates between their builds, each made once, its
# rounds the cases' time alone (TASK-276), and jobs of a matrix share one build, jailed as
# CLADEFS_FIRECRACKER_JAIL says (the nightly's, TASK-284). Neither takes
# CLADEFS_FIRECRACKER_SECCOMP=log, whose filter is built per run. With CLADEFS_E2E_PREBUILT,
# CLADEFS_E2E_HARNESS=DIR takes the harness, the runner and the guest agent, from DIR's artifacts,
# and what it tests, the extent daemon and its tools and Firecracker, from CLADEFS_E2E_PREBUILT's:
# so that one harness, a tree's whose cases LangSmith's pin from before them lacks, drives the
# pin's build and the candidate's alike, a qualification's A/B (scripts/qualify.sh, TASK-330.4).
# CLADEFS_E2E_ROOT names the tree whose code it builds and boots, and whose helpers it runs (default:
# this script's), so that this script's modes serve a tree of before them, an A/B's base.
#
# The guests' extent stores are served by cladefs's extent daemon, which this
# starts, as CLADEFSD tells the tests; its control API is beside it, at
# cladefsd.control, in a directory of their own under /tmp. CLADEFSD_MEMORY
# caps its memory, in bytes. cladefs: every cell of the run is encrypted, under a local KMS of the
# run's (CLADEFS_KMS), and checked so, holding no key, case by case and at the end (TASK-411.15,
# below). LIBCLANG_PATH names libclang's directory where
# llvm-config cannot, and DAX needs access to /dev/userfaultfd (docs/operating-a-host.md).
#
# cladefs: on an arm64 Mac (board 14336), the guests boot on libmachine's Firecracker-compatible VMM
# on Hypervisor.framework (crates/libmachine-firecracker, which this builds, unless
# CLADEFS_FIRECRACKER names a VMM), one level of guest, served by cladefsd, natively; the guest
# agent cross-built for aarch64-unknown-linux-musl (Homebrew's musl-cross: scripts/lib/musl.sh);
# the arm64 guest kernel CLADEFS_GUEST_KERNEL's, else target/arm64-guest/Image
# (crates/libmachine/scripts/arm64-guest.sh fetches it, built on Linux); the root filesystems made
# with Homebrew's e2fsprogs, the cases' images built with podman or docker (OrbStack's), whichever
# runs; the results on a case-sensitive volume (scripts/lib/e2e-macos.sh). What is Linux's alone,
# Firecracker's seccomp filter and jailer, prebuilt artifacts, huge pages and the host's sysctls, it
# refuses; the cases that need Linux say so as they skip (`Test::linux_only`). Needs `brew install
# musl-cross e2fsprogs`.
set -euo pipefail
root=$(cd "${CLADEFS_E2E_ROOT:-$(dirname "$0")/..}" && pwd)
harness="$root/guest/tests"
selectors=("$@")
if (( ${#selectors[@]} == 0 )); then
    selectors=(--tag e2e)
fi
build_only=${CLADEFS_E2E_BUILD_ONLY:-} prebuilt=${CLADEFS_E2E_PREBUILT:-}
if [[ -n $build_only && -n $prebuilt ]]; then
    echo "CLADEFS_E2E_BUILD_ONLY and CLADEFS_E2E_PREBUILT: one or the other" >&2
    exit 2
fi
if [[ -n $build_only$prebuilt && ${CLADEFS_FIRECRACKER_SECCOMP:-} == log ]]; then
    echo "prebuilt artifacts take no CLADEFS_FIRECRACKER_SECCOMP=log" >&2
    exit 2
fi
if [[ -n $prebuilt && ! -f $prebuilt/artifacts.txt ]]; then
    echo "$prebuilt holds no artifacts (CLADEFS_E2E_BUILD_ONLY makes them)" >&2
    exit 2
fi
harness_artifacts=${CLADEFS_E2E_HARNESS:-$prebuilt}
if [[ -n ${CLADEFS_E2E_HARNESS:-} && -z $prebuilt ]]; then
    echo "CLADEFS_E2E_HARNESS goes with CLADEFS_E2E_PREBUILT, whose artifacts it drives" >&2
    exit 2
fi
if [[ -n ${CLADEFS_E2E_HARNESS:-} && ! -f $CLADEFS_E2E_HARNESS/artifacts.txt ]]; then
    echo "$CLADEFS_E2E_HARNESS holds no artifacts (CLADEFS_E2E_BUILD_ONLY makes them)" >&2
    exit 2
fi
if [[ -n $prebuilt && -n ${CLADEFS_FIRECRACKER_JAIL:-} && ! -x $prebuilt/jailer ]]; then
    echo "$prebuilt holds no jailer, built with no seccomp filter: CLADEFS_FIRECRACKER_JAIL needs one" >&2
    exit 2
fi
# Inside a measurement (hotbed run --bench), nothing builds (TASK-368): its artifacts were built as work
# beforehand, every one there; else it refuses, saying so, before anything runs.
source "$(dirname "${BASH_SOURCE[0]}")/lib/measurement.sh"
# user_cache, the downloads' cache, the invoking user's under sudo (board 2720).
source "$(dirname "${BASH_SOURCE[0]}")/lib/user-cache.sh"
if [[ -n $prebuilt ]]; then
    for artifact in "$harness_artifacts/runner" "$harness_artifacts/guest-agent" \
        "$prebuilt/cladefsd" "$prebuilt/cladefsctl" "$prebuilt/cladefs-gc" \
        "$prebuilt/firecracker"; do
        [[ -x $artifact ]] ||
            { echo "$artifact: no such artifact (CLADEFS_E2E_BUILD_ONLY makes them)" >&2; exit 2; }
    done
else
    measurement_refuses "test-e2e.sh${build_only:+ (CLADEFS_E2E_BUILD_ONLY)}, CLADEFS_E2E_PREBUILT unset,"
fi

# The host: Linux, or an arm64 Mac (above), whose guest is arm64; and where the results go, on a
# Mac a case-sensitive volume (scripts/lib/e2e-macos.sh).
os=$(uname -s)
case "$os/$(uname -m)" in
    Linux/*) results_in=${TMPDIR:-/tmp} guest_arch=$(uname -m) ;;
    Darwin/arm64)
        source "$(dirname "${BASH_SOURCE[0]}")/lib/containers.sh"
        source "$(dirname "${BASH_SOURCE[0]}")/lib/e2e-macos.sh"
        for knob in CLADEFS_E2E_BUILD_ONLY CLADEFS_E2E_PREBUILT CLADEFS_E2E_HARNESS \
            CLADEFS_FIRECRACKER_JAIL CLADEFS_GUEST_HUGE_PAGES CLADEFS_MAX_MAP_COUNT \
            CLADEFSD_MEMORY_MAX; do
            [[ -z ${!knob:-} ]] || { echo "$knob is Linux's" >&2; exit 2; }
        done
        [[ ${CLADEFS_FIRECRACKER_SECCOMP:-none} == none ]] ||
            { echo "CLADEFS_FIRECRACKER_SECCOMP is Firecracker's, on Linux" >&2; exit 2; }
        CLADEFS_FIRECRACKER_SECCOMP=none
        # The runner's sockets' directory, as short as a socket's path needs (SUN_LEN), not
        # macOS's own, under /var/folders.
        export TMPDIR=/tmp
        results_in=$(e2e_macos_volume) guest_arch=aarch64
        ;;
    *) echo "cladefs's guests run on Linux and arm64 macOS only: use scripts/test-host.sh" >&2; exit 2 ;;
esac
if [[ -n ${CLADEFS_E2E_RESULTS:-} ]]; then
    results=$CLADEFS_E2E_RESULTS
    mkdir -p "$results"
    [[ $os != Darwin ]] || e2e_macos_case_sensitive "$results"
else
    # Reachable by others, as a jailed VM's user must reach it (below), unlike mktemp's own.
    results=$(mktemp -d "$results_in/cladefs-e2e.XXXXXX")
    chmod 755 "$results"
fi
results=$(cd "$results" && pwd)
if [[ -z ${LIBCLANG_PATH:-} ]] && command -v llvm-config > /dev/null; then
    LIBCLANG_PATH=$(llvm-config --libdir)
fi
if [[ -n ${LIBCLANG_PATH:-} ]]; then export LIBCLANG_PATH; fi

# On a Mac, the guest kernel CLADEFS_GUEST_KERNEL's, or else the arm64 kernel Linux builds, fetched
# (crates/libmachine/scripts/arm64-guest.sh).
if [[ $os == Darwin && -z ${CLADEFS_GUEST_KERNEL:-} ]]; then
    CLADEFS_GUEST_KERNEL=$root/target/arm64-guest/Image
    [[ -f $CLADEFS_GUEST_KERNEL ]] || {
        echo "no $CLADEFS_GUEST_KERNEL: crates/libmachine/scripts/arm64-guest.sh fetches it," \
            "or name one in CLADEFS_GUEST_KERNEL" >&2
        exit 2
    }
    export CLADEFS_GUEST_KERNEL
fi
# cladefs: the guest kernel, CLADEFS_GUEST_KERNEL's, or else this tree's, from the checkout's
# cache of kernels, built there if none of these sources is, only what changed since the last
# (scripts/guest-kernel.sh: in a hotbed workspace, the workspace's, whose base built main's; board
# 7427). Its build's log is the results' guest-kernel.log.
if [[ -z $build_only && -z ${CLADEFS_GUEST_KERNEL:-} ]]; then
    measurement_refuses "the guest kernel's build, CLADEFS_GUEST_KERNEL unset,"
    CLADEFS_GUEST_KERNEL=$(CLADEFS_GUEST_KERNEL_LOG=$results/guest-kernel.log \
        "$root/scripts/guest-kernel.sh") ||
        { echo "building the guest kernel failed: see $results/guest-kernel.log" >&2; exit 1; }
    export CLADEFS_GUEST_KERNEL
fi

# The directory cargo builds into, run in directory $1 as `$2` (default cargo): its workspace's
# target directory, CARGO_TARGET_DIR's if set, as cargo metadata says, not a path guessed here, so
# that a host's target directory of its own (CARGO_TARGET_DIR) is where binaries are taken from
# (TASK-305). Each build below runs in its workspace's directory, as this does, so that cargo
# reads the configuration there.
target_dir() {
    (cd "$1" && "${2:-cargo}" metadata --format-version 1 --no-deps) |
        python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])'
}

# The pjdfstest (BSD-2-Clause) commit pjdfstest-fxfs-guest's cases build, which the runner and the
# guest agent take as they are built: without it, they skip.
source "$root/scripts/lib/pjdfstest.sh"

# The harness's runner, which boots a VM for each test, and says which cases the arguments select,
# a selection naming no case failing before anything else is built (TASK-160.29).
if [[ -n $prebuilt ]]; then
    runner="$harness_artifacts/runner"
else
    (cd "$harness" && cargo build -q --locked -p runner)
    harness_target=$(target_dir "$harness")
    runner="$harness_target/debug/runner"
fi
tests=()
if [[ -z $build_only ]]; then
    listed=$("$runner" list "${selectors[@]}")
    # A line each, as macOS's bash 3.2, which has no mapfile, reads them.
    while IFS= read -r test; do tests+=("$test"); done <<< "$listed"
    repeats=${CLADEFS_E2E_REPEATS:-1}
    [[ $repeats =~ ^[1-9][0-9]*$ ]] || { echo "CLADEFS_E2E_REPEATS is a count" >&2; exit 2; }
    # The run as data, its knobs as it was given them, before this sets any (TASK-160.31).
    "$root/scripts/guest_results.py" start "$results" "$repeats" "${tests[@]}"
    # What a repeat's outcome is called, `repeat`: a runner of before TASK-474, an A/B's base's
    # (CLADEFS_E2E_ROOT), says `round`, as its tree's guest_results.py reads (round-K.json).
    repeat_word=repeat
    runner_test=$("$runner" test --help)
    if [[ $runner_test != *--repeat* ]]; then repeat_word=round; fi
fi

# With frame pointers, as every build here has them (.cargo/config.toml), which the bench's perf
# call graphs walk (CLADEFS_BENCH_PERF). With CLADEFS_FAILPOINTS, a daemon whose failpoints it sets (FAILPOINTS, the fail crate's): requests
# failing or waiting as it says, such as cladefsd::map=1%return(5) (TASK-44.10); with CLADEFS_FAULT_SEED, those of a
# fork's steps its seed says (cladefs::faults); with CLADEFSD_FAILPOINTS=1, one built with them,
# none set, for cases that set their own daemons' (fxfs-guest-soak, TASK-67): in the sim profile
# (release with debug assertions), a test's build, as a release build, a measurement's, refuses
# failpoints (bucket_cell, TASK-320).
cladefsd_features=() cladefsd_profile=release
seccomp=${CLADEFS_FIRECRACKER_SECCOMP:-real}
if [[ -n $prebuilt ]]; then
    cladefsd_bin="$prebuilt/cladefsd"
    export CLADEFSCTL_BIN="$prebuilt/cladefsctl" CLADEFS_GC_BIN="$prebuilt/cladefs-gc"
    export CLADEFS_FIRECRACKER="$prebuilt/firecracker"
    export CLADEFS_GUEST_AGENT="$harness_artifacts/guest-agent"
    if [[ -n ${CLADEFS_FIRECRACKER_JAIL:-} ]]; then
        export CLADEFS_FIRECRACKER_JAILER="$prebuilt/jailer"
    fi
fi
if [[ -z $prebuilt && -n ${CLADEFS_FAILPOINTS:-}${CLADEFS_FAULT_SEED:-}${CLADEFSD_FAILPOINTS:-} ]]; then
    cladefsd_features=(--features failpoints) cladefsd_profile=sim
fi
# The run's directory, by which a case that goes on from repeat to repeat keys what it keeps
# (fxfs-guest-soak).
export CLADEFS_E2E_RUN="$results"
source "$root/scripts/lib/musl.sh"
if [[ $os == Darwin ]]; then
    # cladefs: the daemon and its tools, native (board 13065); the VMM, libmachine's, signed with
    # the hypervisor entitlement as it links (scripts/macos-link.sh), unless CLADEFS_FIRECRACKER
    # names one.
    (cd "$root" && cargo build --locked --profile "$cladefsd_profile" -p cladefs --bin cladefsd \
        --bin cladefsctl ${cladefsd_features[@]+"${cladefsd_features[@]}"})
    (cd "$root" && cargo build --locked --release -p bucket_cell --bin cladefs-gc)
    root_target=$(target_dir "$root")
    cladefsd_bin="$root_target/$cladefsd_profile/cladefsd"
    export CLADEFSCTL_BIN="$root_target/$cladefsd_profile/cladefsctl"
    export CLADEFS_GC_BIN="$root_target/release/cladefs-gc"
    if [[ -z ${CLADEFS_FIRECRACKER:-} ]]; then
        (cd "$root" && cargo build --locked -p libmachine-firecracker)
        export CLADEFS_FIRECRACKER="$root_target/debug/libmachine-firecracker"
    fi
    # mkfs.ext4, for the cases' root filesystems and images.
    PATH=$(e2e_macos_e2fsprogs):$PATH
fi
if [[ -z $prebuilt && $os == Linux ]]; then
musl_setup
(cd "$root" && env "${MUSL_ENV[@]}" cargo build --locked --profile "$cladefsd_profile" \
    --target "$MUSL_TARGET" -p cladefs --bin cladefsd --bin cladefsctl \
    ${cladefsd_features[@]+"${cladefsd_features[@]}"})
root_target=$(target_dir "$root")
cladefsd_bin="$root_target/$MUSL_TARGET/$cladefsd_profile/cladefsd"
# cladefs: cladefsctl, beside it, for cases that run it against the daemon (fxfs-guest-stats,
# TASK-111.4); and the bucket's collector, cladefs-gc, for those that collect their bucket
# (fxfs-guest-soak). All Linux binaries are static musl builds.
export CLADEFSCTL_BIN="$root_target/$MUSL_TARGET/$cladefsd_profile/cladefsctl"
(cd "$root" && env "${MUSL_ENV[@]}" cargo build --locked --release --target "$MUSL_TARGET" \
    -p bucket_cell --bin cladefs-gc)
export CLADEFS_GC_BIN="$root_target/$MUSL_TARGET/release/cladefs-gc"
musl_verify_static "$cladefsd_bin" "$CLADEFSCTL_BIN" "$CLADEFS_GC_BIN"
# As cladefs builds it, with its cladefs feature (scripts/firecracker-cargo.sh), in the copy of
# vendor/firecracker that builds in, whose .cargo/config.toml sets its target directory, unless
# CARGO_TARGET_DIR does, and build environment. CLADEFS_FIRECRACKER_SECCOMP builds Firecracker as production does, for musl, which compiles in its
# filter (TASK-50.1): `real`, the default, runs under that; `log` under a copy of it that allows
# what it would refuse, logging it, which $results/seccomp-log.txt lists afterwards, by thread and
# system call; `none` for the host's target, with no filter of its own, allowing every system
# call. Real by default: copy-on-write DAX's private memory needed three system calls the filter
# lacked, and only jailed runs, which ask for it, would have shown it (TASK-164).
firecracker_cargo="$root/scripts/firecracker-cargo.sh"
firecracker_dir=$("$firecracker_cargo" --tree)
firecracker_target=$(target_dir "$root" "$firecracker_cargo")
case "$seccomp" in
    none)
        "$firecracker_cargo" build --release -p firecracker --features cladefs
        export CLADEFS_FIRECRACKER="$firecracker_target/release/firecracker"
        ;;
    real | log)
        musl="$(uname -m)-unknown-linux-musl"
        # musl-gcc's headers lack the kernel's, which userfaultfd-sys includes: the host's, after
        # musl's own (Firecracker's container has them in musl's tree).
        musl_cflags="CFLAGS_${musl//-/_}=-idirafter /usr/include -idirafter /usr/include/$(uname -m)-linux-gnu"
        env "$musl_cflags" "$firecracker_cargo" build --release -p firecracker --features cladefs \
            --target "$musl"
        export CLADEFS_FIRECRACKER="$firecracker_target/$musl/release/firecracker"
        # Its jailer, for jailed runs of this build, or of its artifacts.
        if [[ -n $build_only || -n ${CLADEFS_FIRECRACKER_JAIL:-} ]]; then
            env "$musl_cflags" "$firecracker_cargo" build --release -p jailer --target "$musl"
            export CLADEFS_FIRECRACKER_JAILER="$firecracker_target/$musl/release/jailer"
        fi
        if [[ $seccomp == log ]]; then
            # The filter the build compiled in: upstream's, with cladefs's rules before each thread's
            # own, as its build.rs puts them (resources/cladefs/README.md).
            python3 - "$firecracker_dir/resources/seccomp/$musl.json" \
                "$firecracker_dir/resources/cladefs/seccomp/$musl.json" "$results/seccomp-log.json" <<'PY'
import json, sys
filters = json.load(open(sys.argv[1]))
for thread, rules in json.load(open(sys.argv[2])).items():
    filters[thread]["filter"][:0] = rules["filter"]
for thread in filters.values():
    thread["default_action"] = "log"
json.dump(filters, open(sys.argv[3], "w"), indent=1)
PY
            "$firecracker_cargo" run --release -q -p seccompiler --bin seccompiler-bin -- \
                --target-arch "$(uname -m)" --input-file "$results/seccomp-log.json" \
                --output-file "$results/seccomp-log.bpf"
            export CLADEFS_FIRECRACKER_SECCOMP_FILTER="$results/seccomp-log.bpf"
            # Every refusal it logs, not the first few of each burst.
            sysctl -q kernel.printk_ratelimit=0
            dmesg --clear
        fi
        ;;
    *) echo "CLADEFS_FIRECRACKER_SECCOMP is real, log or none" >&2; exit 2 ;;
esac
fi
# CLADEFS_FIRECRACKER_JAIL=1 runs each Firecracker as production does (TASK-50.1, TASK-50.4): in
# the jail Firecracker's jailer makes, as an unprivileged user (64000), with /dev/userfaultfd in
# the jail and vm.unprivileged_userfaultfd off; under a seccomp filter, for musl.
if [[ -n ${CLADEFS_FIRECRACKER_JAIL:-} && -z $build_only ]]; then
    [[ $seccomp != none ]] || { echo "CLADEFS_FIRECRACKER_JAIL needs a seccomp filter" >&2; exit 2; }
    export CLADEFS_JAIL_UID=64000 CLADEFS_JAIL_GID=64000
    # The jailed user binds its VM's sockets in the cases' directories, under the results, and
    # their APIs' in the temporary directory: one it can't reach, or write in, failed every case
    # with one, EACCES (TASK-284: GitHub runs' in the runner's home, its user's alone).
    jailed() { setpriv --reuid="$CLADEFS_JAIL_UID" --regid="$CLADEFS_JAIL_GID" --clear-groups "$@"; }
    if ! jailed test -x "$results"; then
        echo "$results is out of the jailed user's reach ($CLADEFS_JAIL_UID)" >&2
        exit 2
    fi
    if ! jailed test -w "${TMPDIR:-/tmp}" -a -x "${TMPDIR:-/tmp}"; then
        echo "${TMPDIR:-/tmp} isn't the jailed user's to write in ($CLADEFS_JAIL_UID)" >&2
        exit 2
    fi
    sysctl -q vm.unprivileged_userfaultfd=0
    # What the tests make, images and all, the jailed user reaches; but the daemons' sockets,
    # their user's alone (TASK-155), which it is given once they serve.
    umask 000
fi
# CLADEFS_MAX_MAP_COUNT sets vm.max_map_count, of which a VM process's DAX window keeps half its
# runs of pages installed (dax::uffd's budget), leaving out those past it (TASK-91.16).
if [[ -n ${CLADEFS_MAX_MAP_COUNT:-} ]]; then
    sysctl -q vm.max_map_count="$CLADEFS_MAX_MAP_COUNT"
    echo "vm.max_map_count: $(sysctl -n vm.max_map_count)"
fi
# CLADEFS_GUEST_HUGE_PAGES=2M backs each VM's memory with the host's 2 MiB huge pages, as many
# reserved as CLADEFS_GUEST_HUGE_PAGES_MIB (default 16384) take (TASK-91.16): the hypervisor's
# second-stage mappings of guest memory then 2 MiB each, what refilling the guest's TLB after a
# flush walks the fewer.
if [[ ${CLADEFS_GUEST_HUGE_PAGES:-} == 2M ]]; then
    sysctl -q vm.nr_hugepages=$(( ${CLADEFS_GUEST_HUGE_PAGES_MIB:-16384} / 2 ))
    echo "huge pages: $(sysctl -n vm.nr_hugepages) of 2 MiB reserved"
elif [[ -n ${CLADEFS_GUEST_HUGE_PAGES:-} ]]; then
    echo "CLADEFS_GUEST_HUGE_PAGES is 2M or unset" >&2; exit 2
fi
# fxfs-guest-docker-dax's Docker, its static binaries (Apache-2.0), downloaded once, and BusyBox,
# its containers' image, the host's static one (busybox-static); fxfs-guest-docker-build-dax's
# too, and Docker's buildx plugin (Apache-2.0), which builds with BuildKit (TASK-311); and
# fxfs-guest-sandboxes', whose builders build so (TASK-330.1).
builds=" fxfs-guest-docker-build-dax fxfs-guest-sandboxes "
# sha256_is SUM FILE fails, saying so, unless FILE's sha256 is SUM: computed and compared here, as
# macOS's sha256sum takes no --check --quiet.
sha256_is() {
    local sum
    sum=$(sha256sum < "$2") || return
    [[ ${sum%% *} == "$1" ]] || { echo "$2: its sha256 is ${sum%% *}, not $1" >&2; return 1; }
}
selected() { # CASE...: whether the run selects one of the CASEs
    local case
    for case in "$@"; do
        [[ " ${tests[*]} " != *" $case "* ]] || return 0
    done
    return 1
}
# shellcheck disable=SC2086 # the cases, a word each
if [[ -z ${CLADEFS_DOCKER_STATIC:-} ]] && selected fxfs-guest-docker-dax $builds; then
    docker_version=29.9.1
    case $guest_arch in
        x86_64) docker_sha256=c158ecc55adb3afb01bbd3539ed3b997d20c26dbdc418080506cc1bf5243ff57 ;;
        aarch64) docker_sha256=3e93cc6d1a686456a3b18eefdde26467a837a7a2b257af22e27cba1dbc1b2018 ;;
    esac
    docker_dir=$(user_cache "docker-$docker_version")
    if [[ ! -x $docker_dir/dockerd ]]; then
        curl -fsSL -o "$docker_dir.tgz" \
            "https://download.docker.com/linux/static/stable/$guest_arch/docker-$docker_version.tgz"
        sha256_is "$docker_sha256" "$docker_dir.tgz"
        tar -xzf "$docker_dir.tgz" -C "$docker_dir" --strip-components=1
        rm "$docker_dir.tgz"
    fi
    if [[ -z ${CLADEFS_BUSYBOX:-} && $os == Darwin ]]; then
        CLADEFS_BUSYBOX=$(e2e_macos_busybox)
    fi
    export CLADEFS_DOCKER_STATIC="$docker_dir" CLADEFS_BUSYBOX="${CLADEFS_BUSYBOX:-/bin/busybox}"
fi
# shellcheck disable=SC2086 # the cases, a word each
if [[ -z ${CLADEFS_DOCKER_BUILDX:-} ]] && selected $builds; then
    buildx_version=0.38.2
    case $guest_arch in
        x86_64) buildx_arch=amd64 buildx_sha256=7992ec09413b80ff57ba4e081f56fc833d7bac1d5e71143c6c809e7e6065f6d6 ;;
        aarch64) buildx_arch=arm64 buildx_sha256=46c0f334772de3cb0b341cc415caf7339fb5cfc8b8438ebcb5a10e7f23800d36 ;;
    esac
    buildx=$(user_cache)/docker-buildx-$buildx_version
    if [[ ! -x $buildx ]]; then
        curl -fsSL -o "$buildx.download" "https://github.com/docker/buildx/releases/download/v$buildx_version/buildx-v$buildx_version.linux-$buildx_arch"
        sha256_is "$buildx_sha256" "$buildx.download"
        chmod +x "$buildx.download"
        mv "$buildx.download" "$buildx"
    fi
    export CLADEFS_DOCKER_BUILDX="$buildx"
fi
# The harness's guest agent, a static binary each guest runs as its init. pjdfstest's cases take
# its repository and commit as they build.
guest_target="$guest_arch-unknown-linux-musl"
if [[ -z $build_only$prebuilt ]]; then
    # The harness's own tests, scripts/test.sh's guest part, which as another user than root says
    # it leaves out those that need root (TASK-307); and the VMM's extent device's tests, and its
    # vCPUs' exits', cladefs's, which no other suite reaches (TASK-79.102, TASK-187): its firecracker
    # part, which CI runs too (TASK-220). On a Mac, the harness's host half's (Firecracker is
    # Linux's).
    test_parts="guest firecracker"
    [[ $os == Linux ]] || test_parts=guest
    # Without a case's replay knobs, which the harness's own tests of scripts read and fail on
    # (a_hang_is_shrunk, with CLADEFS_RANDOM_OPS_DROP set): a script's replay line runs as said.
    env -u CLADEFS_SEED -u CLADEFS_RANDOM_OPS_DROP "CLADEFS_TEST_PARTS=$test_parts" "$root/scripts/test.sh"
fi
if [[ -z $prebuilt ]]; then
    # Cross-built on a Mac, with a musl cross toolchain (scripts/lib/musl.sh); natively on Linux.
    guest_env=()
    if [[ $os != Linux ]]; then
        musl_setup "$guest_target"
        guest_env=("${MUSL_ENV[@]}")
    fi
    (cd "$harness" && env ${guest_env[@]+"${guest_env[@]}"} cargo build -q --locked \
        --target "$guest_target" -p guest-agent)
    export CLADEFS_GUEST_AGENT="$harness_target/$guest_target/debug/guest-agent"
fi
# cladefs: the artifacts, copied for runs to take as they are (CLADEFS_E2E_PREBUILT), and what they
# were built from and as; and nothing run.
if [[ -n $build_only ]]; then
    mkdir -p "$build_only"
    artifacts=(runner guest-agent cladefsd cladefsctl cladefs-gc firecracker)
    cp "$runner" "$CLADEFS_GUEST_AGENT" "$cladefsd_bin" "$CLADEFSCTL_BIN" "$CLADEFS_GC_BIN" \
        "$CLADEFS_FIRECRACKER" "$build_only/"
    if [[ -n ${CLADEFS_FIRECRACKER_JAILER:-} ]]; then
        cp "$CLADEFS_FIRECRACKER_JAILER" "$build_only/"
        artifacts+=(jailer)
    fi
    {
        echo "source $(cat "$root/source-revision.txt" 2> /dev/null || echo unknown)"
        echo "seccomp $seccomp"
        (cd "$build_only" && sha256sum "${artifacts[@]}")
    } > "$build_only/artifacts.txt"
    echo "Artifacts: $build_only" >&2
    exit 0
fi
# Its sockets in a directory of their own under /tmp, not the results': a socket's path is
# 107 bytes at most (SUN_LEN), which a host run's results nested deeper passed, and the daemon
# exited ("listening at .../results/main-r1/sandboxes/cladefsd.control: path must be shorter than
# SUN_LEN", TASK-330.4's A/B). Reachable by others, as a jailed VM's user must reach the socket.
sockets=$(mktemp -d /tmp/cladefs-e2e-sockets.XXXXXX)
chmod 755 "$sockets"
socket="$sockets/cladefsd.sock"
control="$sockets/cladefsd.control"
# cladefs: the run's cells encrypted, as production's tenants' are (TASK-411.15, doc-16): the guest
# tests run encrypted only, plaintext covered by the host's suites. A local KMS, beside the sockets,
# gone with them, not in the results, which runs keep, holding a KEK for every tenant the cases use,
# the default tenant (0) among them (the runner's `kms`); CLADEFS_KMS names it to every process of
# the run, the daemon, the cases' own daemons, cladefsctl, cladefs-gc and FUSE mounts, unless it
# names one already, whose tenants' KEKs are the caller's to make. CLADEFS_E2E_ENCRYPTED has the
# runner check each case's cells, once it passed, holding no key (test_cases::encrypted): its cells'
# KEKs, its records' DEKs, its objects' and files' encryptions, and no known plaintext anywhere; and
# the run's own bucket and part cache at its end. Every cell is encrypted since TASK-411.28: its
# daemon, cladefsctl, FUSE mounts and runner refuse to start without a KMS; cladefs-gc holds no key
# (doc-27). A daemon and harness both built before encryption (an A/B's old side,
# CLADEFS_E2E_PREBUILT, CLADEFS_E2E_ROOT, of an old pin) run in the clear, unchecked, saying so; one
# of them old and the other not is refused, as neither reads the other's cells: what each run was is
# in encryption.txt.
kms_dir="$sockets/kms"
old_daemon='' old_runner=''
grep -qa CLADEFS_KMS "$cladefsd_bin" || old_daemon=1
"$runner" kms --help > /dev/null 2>&1 || old_runner=1
if [[ -n $old_daemon && -n $old_runner ]]; then
    echo "clear: $cladefsd_bin and $runner predate encryption (no CLADEFS_KMS, no \`runner kms\`)" \
        > "$results/encryption.txt"
elif [[ -n $old_daemon$old_runner ]]; then
    echo "$cladefsd_bin and $runner: one predates encryption, the other encrypts every cell" \
        "(TASK-411.28): run a daemon and a harness of one pin" >&2
    exit 1
else
    if [[ -z ${CLADEFS_KMS:-} ]]; then
        "$runner" kms "$kms_dir"
        export CLADEFS_KMS="local://$kms_dir"
    fi
    export CLADEFS_E2E_ENCRYPTED=1
    echo "encrypted: $CLADEFS_KMS" > "$results/encryption.txt"
fi
echo "encryption: $(cat "$results/encryption.txt")" >&2
# A host keeps cells in a bucket, CLADEFSD_BUCKET's or a file bucket in memory, on tmpfs,
# as of placement 1 unless one is named; what it did with it scripts/bench-report.py reports. Not
# one in the daemon's memory: the cases open the cells it made in processes of their own (cladefs's
# Storage), which keep a cell in the bucket its description names (TASK-165.6). Its
# memory is CLADEFSD_MEMORY bytes, if set; otherwise, as production's, a page cache of what
# of the host's memory, and the run's cgroup's, is free (TASK-358).
# cladefs: what its cells read from the bucket it keeps in a part cache of its own on disk, as
# production's daemon does (CLADEFSD_PART_CACHE, doc-7), unless CLADEFSD_PART_CACHE says
# otherwise, empty for none; not this process's, which the cases' own daemons would share
# (TASK-164.12). In the results' directory, where the cases make their cells: a cell goes into
# the cache by renames, which a cell on another filesystem than the cache's refuses (TASK-359).
# What the run's daemon's stats file and log are called, NAME-stats.txt and NAME.log: as the
# tree's own helpers read them (scripts/guest_results.py), cladefsd, or the daemon's name before
# TASK-503 in a tree from before it (CLADEFS_E2E_ROOT, an A/B's base).
daemon_file=$(grep -om1 '[a-z]*d-stats\.txt' "$root/scripts/guest_results.py" || true)
daemon_file=${daemon_file:-cladefsd-stats.txt}
daemon_file=${daemon_file%-stats.txt}
bucket_dir=
if [[ -z ${CLADEFSD_BUCKET:-} && $os == Darwin ]]; then
    # On a Mac, which has no /dev/shm nor flock(1), on the run's volume, removed as the run exits.
    bucket_dir=$(mktemp -d "$results_in/cladefs-e2e-bucket.XXXXXX")
elif [[ -z ${CLADEFSD_BUCKET:-} ]]; then
    # In memory, owned: the next run removes it if this one is killed outright (e2e-bucket.sh).
    source "$(dirname "${BASH_SOURCE[0]}")/lib/e2e-bucket.sh"
    e2e_bucket /dev/shm
    bucket_dir=$e2e_bucket_dir
fi
cache_dir=
if [[ -z ${CLADEFSD_PART_CACHE+set} ]]; then
    cache_dir=$(mktemp -d "$results/.cladefsd-cache.XXXXXX")
fi
disk_dir=$(mktemp -d "$results/.cladefsd-disk0.XXXXXX")
chmod 755 "$disk_dir"
# cladefs: CLADEFSD_MEMORY_MAX runs the daemon in a systemd scope of its own, limited to that
# many bytes of memory (systemd's MemoryMax, no swap), its cgroup killing it past them, and keeps
# the most it held, memory.peak, in cladefsd-memory-peak.txt as the run ends: what the daemon's
# memory budget is measured against (TASK-317). CLADEFSD_WRAP, a command and its arguments,
# runs the daemon under it, such as a heap profiler (`heaptrack -o DIR/heap`).
cladefsd_scope=
cladefsd_run=()
if [[ -n ${CLADEFSD_MEMORY_MAX:-} ]]; then
    cladefsd_scope="cladefs-e2e-cladefsd-$$.scope"
    cladefsd_run=(systemd-run --scope --quiet --collect --unit="$cladefsd_scope"
        -p MemoryMax="$CLADEFSD_MEMORY_MAX" -p MemorySwapMax=0 --)
fi
read -r -a cladefsd_wrap <<< "${CLADEFSD_WRAP:-}"
# The most the daemon's scope held, kept before the daemon goes, whose scope goes with it; and the
# scope stopped, its every process with it, as a wrapper (heaptrack's script) may outlive the kill.
cladefsd_peak() {
    [[ -n $cladefsd_scope ]] || return 0
    systemctl show -P MemoryPeak "$cladefsd_scope" > "$results/cladefsd-memory-peak.txt" || true
}
cladefsd_stop() {
    [[ -n $cladefsd_scope ]] || return 0
    systemctl stop "$cladefsd_scope" 2> /dev/null || true
}
# cladefs: on hosts without Linux MGLRU/governor support (e.g. hotbed VMs with
# /sys/kernel/mm/lru_gen/enabled=0 or insufficient swap), pass an explicit test memory budget so
# non-governor e2e tests run while production cladefsd keeps failing hard in governed mode
# (board 13373, board 13393).
cladefsd_memory=${CLADEFSD_MEMORY:-}
if [[ -z $cladefsd_memory ]]; then
    mglru_enabled=0
    if [[ -r /sys/kernel/mm/lru_gen/enabled ]]; then
        mglru_raw=$(tr -d '[:space:]' < /sys/kernel/mm/lru_gen/enabled 2> /dev/null || echo 0)
        if [[ $mglru_raw =~ ^0[xX][0-9a-fA-F]+$ ]]; then
            (( 16#${mglru_raw#0[xX]} != 0 )) && mglru_enabled=1
        elif [[ $mglru_raw =~ ^[0-9]+$ ]]; then
            (( 10#$mglru_raw != 0 )) && mglru_enabled=1
        fi
    fi
    swap_kb=$(awk '/^SwapTotal:/ { print $2; exit }' /proc/meminfo 2> /dev/null || echo 0)
    if (( mglru_enabled == 0 || swap_kb == 0 )); then
        cladefsd_memory=$((1 << 30))
    fi
fi
# The run's bucket, which the daemon keeps its cells in, named to the cases too: a case whose
# daemon of its own forks a version the run's daemon made reads it there (bench-fork's cold and
# predicted forks).
export CLADEFS_E2E_BUCKET="${CLADEFSD_BUCKET:-file://$bucket_dir}"
${cladefsd_run[@]+"${cladefsd_run[@]}"} \
env CLADEFSD_BUCKET="$CLADEFS_E2E_BUCKET" \
    CLADEFSD_ROOTS="$disk_dir" \
    CLADEFSD_PART_CACHE="${CLADEFSD_PART_CACHE-$cache_dir}" \
    CLADEFSD_TOKEN="${CLADEFSD_TOKEN:-1}" \
    CLADEFSD_VM_CGROUP_DELEGATED="${CLADEFSD_VM_CGROUP_DELEGATED:-1}" \
    CLADEFSD_STATS="$results/$daemon_file-stats.txt" CLADEFSD_GRPC="$control" \
    FAILPOINTS="${CLADEFS_FAILPOINTS:-}" ${cladefsd_wrap[@]+"${cladefsd_wrap[@]}"} "$cladefsd_bin" "$socket" ${cladefsd_memory:+"$cladefsd_memory"} \
    > "$results/$daemon_file.log" 2>&1 &
daemon=$!
trap 'cladefsd_peak; kill "$daemon" 2> /dev/null; cladefsd_stop; rm -r "$sockets"; [[ -z $bucket_dir ]] || rm -r "$bucket_dir"; [[ -z $cache_dir ]] || rm -r "$cache_dir"; rm -r "$disk_dir"' EXIT
while [[ ! -S $socket || ! -S $control ]]; do
    if ! kill -0 "$daemon" 2> /dev/null; then
        echo "the extent daemon exited: see $results/$daemon_file.log" >&2
        exit 1
    fi
    sleep 0.1
done
# The jailed Firecrackers use the default tenant's socket, as production's use a tenant's.
if [[ -n ${CLADEFS_FIRECRACKER_JAIL:-} ]]; then
    chown "$CLADEFS_JAIL_UID:$CLADEFS_JAIL_GID" "$socket"
fi
export CLADEFSD="$socket"
export CLADEFSD_ROOTS="$disk_dir"
# cladefs: the daemon's binary, for cases that start daemons of their own.
export CLADEFSD_BIN="$cladefsd_bin"

status=0
for test in "${tests[@]}"; do
    for (( repeat = 1; repeat <= repeats; repeat++ )); do
        # A single repeat's logs in the case's directory, where scripts/pgbench-gate.py reads them;
        # each of several repeats' in one of its own.
        base="$results/$test"
        (( repeats == 1 )) || base+="/repeat-$repeat"
        passed=1
        "$runner" test --test-case "$test" --base-dir "$base" --keep-all \
            --outcome "$results/$test/$repeat_word-$repeat.json" --run "$results/run.json" \
            "--$repeat_word" "$repeat" || passed=0
        if (( passed && repeats > 1 )); then
            find "$base" -name log.txt -exec cp {} "$results/$test/repeat-$repeat.log" \;
            # Writable first: a root filesystem extracted keeps its read-only directories, which
            # a user not root, as on a Mac, can't empty (/root, /etc/pki's), failing the run.
            chmod -R u+w "$base"
            rm -r "$base"
        fi
        "$root/scripts/guest_results.py" ended "$results" "$test"
        if (( ! passed )); then
            echo "$test: failed in repeat $repeat of $repeats: see $base" >&2
            status=1
            break
        fi
    done
    if (( passed && repeats > 1 )); then
        echo "$test: $repeats repeats passed" >&2
    fi
done
# cladefs: the run's own bucket, whole, and its part cache, encrypted and holding no known plaintext,
# as each case's cells were (TASK-411.15).
if [[ -n ${CLADEFS_E2E_ENCRYPTED:-} ]] &&
    ! "$runner" encrypted --bucket "${CLADEFSD_BUCKET:-file://$bucket_dir}" \
        "$disk_dir" ${cache_dir:+"$cache_dir"} 2> "$results/encrypted.txt"; then
    echo "the run's bucket or part cache isn't encrypted: see $results/encrypted.txt; the first:" >&2
    sed -n 1,3p "$results/encrypted.txt" >&2
    status=1
fi
if [[ $seccomp == log ]]; then
    # The audit records of the system calls the filter would refuse: thread, call, count.
    dmesg | python3 -c '
import collections, re, sys
seen = collections.Counter()
for line in sys.stdin:
    if "type=1326" not in line:
        continue
    # A name with a space, as vCPU threads have, is hexadecimal, unquoted.
    comm = re.search(r"comm=(?:\"([^\"]*)\"|([0-9A-F]+)\b)", line)
    call = re.search(r"syscall=(\d+)", line)
    if comm and call:
        name = comm.group(1) or bytes.fromhex(comm.group(2)).decode(errors="replace")
        seen[(name, int(call.group(1)))] += 1
for (comm, call), count in sorted(seen.items()):
    print(f"{comm}\t{call}\t{count}")
' > "$results/seccomp-log.txt"
    echo "seccomp: $(wc -l < "$results/seccomp-log.txt") thread and call pairs refused" >&2
fi
# cladefs: the guest writes the DAX windows refused: the guest and the window disagreeing, which fails
# the run (TASK-44.12). Not a kernel worker's, the VM processes' logs saying whose (TASK-150.16).
firecracker_logs=()
while IFS= read -r log; do firecracker_logs+=("$log"); done < <(find "$results" -name firecracker.log)
if ! "$root/scripts/dax-refusals.py" "$results/$daemon_file.log" \
    ${firecracker_logs[@]+"${firecracker_logs[@]}"} \
    > "$results/unexplained-refusals.txt"; then
    echo "the DAX window refused a guest's writes: see $results/unexplained-refusals.txt" >&2
    status=1
fi
# cladefs: the bucket's requests that kept failing, as the daemon logged them (TASK-314): each is
# retried until it is answered, holding its guests' commits at the lag bound meanwhile. The run's
# own file bucket, on tmpfs, fails only once that is full, its cases then hanging until the runner
# times them out: the run fails, saying so.
grep -E "object storage: [a-z-]+ requests (failing|still failing)" "$results/$daemon_file.log" \
    > "$results/bucket-failures.txt" || true
if [[ -s $results/bucket-failures.txt ]]; then
    echo "the bucket failed requests, each retried: see $results/bucket-failures.txt; the first:" >&2
    sed -n 1p "$results/bucket-failures.txt" >&2
    if [[ -n $bucket_dir ]]; then
        df -h "$bucket_dir" >&2 || true
        status=1
    fi
else
    rm "$results/bucket-failures.txt"
fi
# cladefs: what each extent daemon cost, against its design's budgets (TASK-330.2): its anonymous
# memory, its uploads' memory, and its disk's writes, each what it says it wrote; and what it wrote
# for each guest byte, by kind. The run's own daemon's bucket, a file bucket on tmpfs, writes no
# disk.
budgets=()
[[ -z $bucket_dir ]] || budgets=(--bucket-in-memory "$results/$daemon_file-stats.txt")
if ! "$root/scripts/budgets.py" ${budgets[@]+"${budgets[@]}"} "$results"; then
    echo "an extent daemon past its budget: see $results/budgets.json" >&2
    status=1
fi
echo "Logs: $results" >&2
"$root/scripts/guest_results.py" finish "$results" "$status"
exit "$status"
