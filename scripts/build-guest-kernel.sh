#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Build the guest kernel with Fxfs in it (doc-2, TASK-26.1): Amazon Linux's 6.18 kernel, the
# release scripts/lib/guest-kernel-release.sh names by its tag and the commit the tag must name,
# configured as Firecracker's CI configures the 6.18 guests it supports (its configurations, in
# guest/linux/configs, and its docs/kernel-policy.md), with cladefs's series of
# patches (guest/linux/patches) adding fs/cladefs and drivers/virtio_extent from guest/linux, and the
# filesystem's Rust, which cargo builds from guest/rust, linked into fs/cladefs (TASK-39.6).
#
#   build-guest-kernel.sh [arm64|x86_64]
#
# builds for this machine's architecture by default. It runs on Linux, natively (on a test host:
# scripts/guest-kernel.sh). The sources and the build live under
# $CLADEFS_GUEST_KERNEL_WORK (default: ~/cladefs-guest-kernel), and its out/<arch> gets the kernel
# (Image for arm64, vmlinux for x86-64, as Firecracker loads them) and what builds it elsewhere, on
# the same release: patches/, the series (git am); libcladefs_linux.a, the Rust, for fs/cladefs; and
# cladefs.config, cladefs's options over the configuration. CLADEFS_GUEST_KERNEL_PROFILE (below)
# builds one for profiling, CLADEFS_GUEST_KERNEL_LOCK_STAT one counting locks' contention,
# CLADEFS_GUEST_KERNEL_DEBUG one checking its locking and races, CLADEFS_GUEST_KERNEL_KASAN one
# checking its memory accesses, CLADEFS_GUEST_KERNEL_RUST_CHECKS one whose Rust checks itself.
# CLADEFS_GUEST_KERNEL_JOBS is make's jobs (default: the CPUs), fewer on a host others share (the
# test host), its Rust's cargo's CARGO_BUILD_JOBS's.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=${CLADEFS_GUEST_KERNEL_WORK:-$HOME/cladefs-guest-kernel}
# The release, its tag and the commit it must name, and the date the build is stamped with.
source "$root/scripts/lib/guest-kernel-release.sh"
configs="$root/guest/linux/configs"

arch=${1:-$(uname -m)}
case "$arch" in
    arm64 | aarch64)
        arch=arm64 guestarch=aarch64 target=aarch64-unknown-none-softfloat image=arch/arm64/boot/Image
        make_target=Image
        ;;
    x86_64)
        arch=x86_64 guestarch=x86_64 target=x86_64-unknown-none image=vmlinux make_target=vmlinux
        ;;
    *) echo "Unsupported architecture: $arch" >&2; exit 2 ;;
esac
make_args=(ARCH="$arch")
strip_bin=strip
if [[ $guestarch != $(uname -m) ]]; then
    make_args+=(CROSS_COMPILE="$guestarch-linux-gnu-")
    strip_bin="$guestarch-linux-gnu-strip"
fi

# The kernel's sources, once, and the release's commit, whether cloned now or before: a tag that
# moved, or a tree left otherwise, is refused (board 8601).
src="$work/$kernel_tag-$arch"
if [[ ! -d $src ]]; then
    git clone -q --depth 1 --branch "$kernel_tag" https://github.com/amazonlinux/linux "$src"
fi
kernel_release_check "$src"
out="$work/out/$arch"
mkdir -p "$out"

# cladefs's changes to the kernel, a commit each on its release: the series of guest/linux/patches,
# each with the files of guest/linux it adds. The tree checks out the series' last commit, which
# rewrites only the files that differ from what the tree has, so that a build after the first
# rebuilds only what changed; and the series, as patches, goes to out/<arch>/patches, which a
# kernel built elsewhere applies (git am) on the same release.
kgit() { git -C "$src" "$@"; }
export GIT_AUTHOR_NAME="The cladefs Authors" GIT_AUTHOR_EMAIL="cladefs@echophase.com"
export GIT_COMMITTER_NAME=$GIT_AUTHOR_NAME GIT_COMMITTER_EMAIL=$GIT_AUTHOR_EMAIL
# The same series, the same commits.
export GIT_AUTHOR_DATE=$kernel_build_date GIT_COMMITTER_DATE=$kernel_build_date
index="$work/series-index-$arch"
series=$kernel_commit
GIT_INDEX_FILE=$index kgit read-tree "$series"
# commit PATCH [PATH...]: the next commit of the series, PATCH's change and guest/linux's PATHs
# (files, or directories, without what builds leave there), its message PATCH's header.
commit() {
    local patch="$root/guest/linux/patches/$1" path file blob tree
    shift
    GIT_INDEX_FILE=$index kgit apply --cached "$patch"
    for path in "$@"; do
        (cd "$root/guest/linux" && find "$path" -type f ! -name '*.o' ! -name '.*.cmd' \
            ! -name '*.a' | sort) | while read -r file; do
            blob=$(kgit hash-object -w "$root/guest/linux/$file")
            GIT_INDEX_FILE=$index kgit update-index --add --cacheinfo "100644,$blob,$file"
        done
    done
    tree=$(GIT_INDEX_FILE=$index kgit write-tree)
    # The header, but for its license lines, up to the diff.
    series=$(sed -n '/^diff --git /q; /^# /d; p' "$patch" | kgit commit-tree "$tree" -p "$series")
}
commit 0003-virtio-extent.patch drivers/virtio_extent include/linux/virtio_extent.h
commit 0004-cladefs.patch fs/cladefs
commit 0010-dax-read-replaces-no-entry-naming-its-page.patch
commit 0011-dax-a-folio-put-for-good-keeps-no-share-count.patch
commit 0014-fs-rotate-inodes-whose-page-cache-could-not-be-dropped.patch
commit 0015-fs-dont-lose-inodes-another-lru-walker-is-isolating.patch
rm -f "$index"
kgit reset -q --hard "$series"
rm -f "$out"/patches/*.patch
kgit format-patch -q --zero-commit --no-signature -o "$out/patches" "$kernel_commit..$series"

# Firecracker's configuration of its 6.18 guests, its guests' and its CI's, as its
# resources/rebuild.sh has them, and cladefs's options, the later overriding the earlier. cladefs.config
# goes to out/<arch> too.
awk 1 "$configs/microvm-kernel-ci-$guestarch-$kernel_series.config" "$configs/ci.config" \
    "$configs/cladefs.config" > "$src/.config"
cp "$configs/cladefs.config" "$out/"
# With CLADEFS_GUEST_KERNEL_PROFILE=1, an x86-64 kernel unwinds its stacks by frame pointers, which
# Fxfs's Rust keeps, so that perf's call graphs follow calls through it, not by guesses.
# Frame pointers cost the kernel's own code a little, so a kernel measured has none: build one
# for profiling with its own CLADEFS_GUEST_KERNEL_WORK.
if [[ $arch == x86_64 && ${CLADEFS_GUEST_KERNEL_PROFILE:-} == 1 ]]; then
    "$src/scripts/config" --file "$src/.config" --disable UNWINDER_GUESS \
        --enable UNWINDER_FRAME_POINTER
fi
# cladefs: with CLADEFS_GUEST_KERNEL_DEBUG=1, a kernel that checks its locking (lockdep, sleeping in
# atomic context) and, with KCSAN, its data races, and that fails allocations as a test asks, through debugfs
# (failslab, fail_page_alloc: fxfs-guest-faults-dax, TASK-44.10): slow, so built with its own
# CLADEFS_GUEST_KERNEL_WORK.
if [[ ${CLADEFS_GUEST_KERNEL_DEBUG:-} == 1 ]]; then
    "$src/scripts/config" --file "$src/.config" --enable DEBUG_KERNEL --enable PROVE_LOCKING \
        --enable DEBUG_ATOMIC_SLEEP --enable DEBUG_LIST --enable KCSAN --enable DEBUG_FS \
        --enable FAULT_INJECTION --enable FAILSLAB --enable FAIL_PAGE_ALLOC \
        --enable FAULT_INJECTION_DEBUG_FS --enable FAULT_INJECTION_STACKTRACE_FILTER
fi
# cladefs: with CLADEFS_GUEST_KERNEL_KASAN=1, a kernel that checks its memory accesses (KASAN, and
# KFENCE's sampled checks) and its locking, which KCSAN's can't be built with (TASK-44.8): slow,
# so built with its own CLADEFS_GUEST_KERNEL_WORK. Booted at its 64-bit entry, not PVH's, which
# Firecracker takes a kernel's Xen note for: entered there, a KASAN kernel faulted before its
# first line, the VM shut down within a millisecond (GCE r84, r253; it boots without, r256).
if [[ ${CLADEFS_GUEST_KERNEL_KASAN:-} == 1 ]]; then
    "$src/scripts/config" --file "$src/.config" --enable DEBUG_KERNEL --enable KASAN \
        --enable KASAN_GENERIC --enable KFENCE --enable PROVE_LOCKING --enable DEBUG_ATOMIC_SLEEP \
        --enable DEBUG_LIST --disable KCSAN --disable PVH
fi
# cladefs: with CLADEFS_GUEST_KERNEL_LOCK_STAT=1, a kernel that counts each lock's contention, in
# /proc/lock_stat, which names a hot lock where a sampling profile can't see it, taken with
# interrupts off (fxfs-guest-pgbench-locks, TASK-91.14): slow, so built with its own
# CLADEFS_GUEST_KERNEL_WORK.
if [[ ${CLADEFS_GUEST_KERNEL_LOCK_STAT:-} == 1 ]]; then
    "$src/scripts/config" --file "$src/.config" --enable DEBUG_KERNEL --enable LOCK_STAT
fi
# cladefs: with CLADEFS_GUEST_KERNEL_COVERAGE=1, a kernel that counts what fs/cladefs runs (gcov, which
# fs/cladefs/Makefile asks of its C), for scripts/guest-coverage.py (TASK-119.14): built with its own
# CLADEFS_GUEST_KERNEL_WORK.
if [[ ${CLADEFS_GUEST_KERNEL_COVERAGE:-} == 1 ]]; then
    "$src/scripts/config" --file "$src/.config" --enable DEBUG_FS --enable GCOV_KERNEL
fi
make -C "$src" "${make_args[@]}" olddefconfig
if [[ ${CLADEFS_GUEST_KERNEL_LOCK_STAT:-} == 1 ]]; then
    grep -q '^CONFIG_LOCK_STAT=y' "$src/.config" ||
        { echo "the lock_stat kernel has no CONFIG_LOCK_STAT" >&2; exit 1; }
fi
if [[ ${CLADEFS_GUEST_KERNEL_DEBUG:-} == 1 ]]; then
    for option in PROVE_LOCKING DEBUG_ATOMIC_SLEEP KCSAN FAILSLAB FAIL_PAGE_ALLOC \
        FAULT_INJECTION_DEBUG_FS FAULT_INJECTION_STACKTRACE_FILTER; do
        grep -q "^CONFIG_$option=y" "$src/.config" ||
            { echo "the debug kernel has no CONFIG_$option" >&2; exit 1; }
    done
fi
if [[ ${CLADEFS_GUEST_KERNEL_KASAN:-} == 1 ]]; then
    for option in KASAN KFENCE PROVE_LOCKING; do
        grep -q "^CONFIG_$option=y" "$src/.config" ||
            { echo "the KASAN kernel has no CONFIG_$option" >&2; exit 1; }
    done
fi
if [[ ${CLADEFS_GUEST_KERNEL_COVERAGE:-} == 1 ]]; then
    grep -q '^CONFIG_GCOV_KERNEL=y' "$src/.config" ||
        { echo "the coverage kernel has no CONFIG_GCOV_KERNEL" >&2; exit 1; }
fi
grep -q '^CONFIG_CLADEFS_FS=y' "$src/.config"
grep -q '^CONFIG_VIRTIO_EXTENT=y' "$src/.config"
grep -q '^CONFIG_VIRTIO_PMEM=y' "$src/.config"
grep -q '^CONFIG_SERIAL_8250_CONSOLE=y' "$src/.config"
# cladefs: fs/cladefs's tracepoints, in tracefs and for BPF (cladefs.config, TASK-44.16).
grep -q '^CONFIG_FTRACE=y' "$src/.config"
grep -q '^CONFIG_BPF_EVENTS=y' "$src/.config"
# cladefs: eBPF JIT, kprobes, dynamic ftrace, syscall tracepoints, BTF, and schedstats for in-guest profiling/bpftrace.
grep -q '^CONFIG_BPF_JIT=y' "$src/.config"
grep -q '^CONFIG_KPROBES=y' "$src/.config"
grep -q '^CONFIG_KPROBE_EVENTS=y' "$src/.config"
grep -q '^CONFIG_DYNAMIC_FTRACE=y' "$src/.config"
grep -q '^CONFIG_FTRACE_SYSCALLS=y' "$src/.config"
grep -q '^CONFIG_DEBUG_INFO_BTF=y' "$src/.config"
grep -q '^CONFIG_SCHEDSTATS=y' "$src/.config"
# cladefs: an oops panics, and a panic resets the VM (cladefs.config, TASK-311).
grep -q '^CONFIG_PANIC_ON_OOPS=y' "$src/.config"
grep -q '^CONFIG_PANIC_TIMEOUT=-1' "$src/.config"
# cladefs: nested KVM, so guests on hosts with nested virtualization expose /dev/kvm.
grep -q '^CONFIG_VIRTUALIZATION=y' "$src/.config"
grep -q '^CONFIG_KVM=y' "$src/.config"
if [[ $arch == x86_64 ]]; then
    grep -q '^CONFIG_KVM_AMD=y' "$src/.config"
    grep -q '^CONFIG_KVM_INTEL=y' "$src/.config"
fi
# libmachine's platform (doc-30, board 13061), which Firecracker's configuration sets but for
# CPU_FREQ: on x86-64 an ELF vmlinux entered through PVH, hardware-reduced ACPI (which needs PCI to
# load its DSDT), the console a 16550 that only the DSDT describes (PNP0501), every device on
# virtio-PCI with MSI-X, the agent's vsock and the network's card configured by ip=, and the VM
# generation ID a restored clone reseeds its RNG from.
if [[ $arch == x86_64 ]]; then
    for option in PVH ACPI PCI PNPACPI SERIAL_8250_PNP PCI_MSI VIRTIO_PCI VIRTIO_VSOCKETS IP_PNP \
        VIRTIO_NET VMGENID; do
        grep -q "^CONFIG_$option=y" "$src/.config" ||
            { echo "libmachine's platform needs CONFIG_$option" >&2; exit 1; }
    done
    ! grep -q '^CONFIG_CPU_FREQ=y' "$src/.config"
fi
# cladefs: compressed in-RAM swap (/dev/zram0) inside guests.
grep -q '^CONFIG_ZRAM=y' "$src/.config"
grep -q '^CONFIG_ZRAM_BACKEND_ZSTD=y' "$src/.config"
grep -q '^CONFIG_ZRAM_BACKEND_LZ4=y' "$src/.config"

# The filesystem's Rust, against a sysroot whose std is the kernel's (guest/rust/std). With
# CLADEFS_GUEST_KERNEL_RUST_CHECKS=1, optimized, but with its debug assertions, and arithmetic that
# panics if it overflows: the release build's silence hid a transaction larger than it reserved
# for, and a reservation computed from a subtraction that wrapped (TASK-149.1). Built with its own
# CLADEFS_GUEST_KERNEL_WORK. Both always set, false being the release profile's own: an empty array
# of them is unbound to macOS's bash 3.2 under `set -u`.
rust_checks=false
if [[ ${CLADEFS_GUEST_KERNEL_RUST_CHECKS:-} == 1 ]]; then
    rust_checks=true
fi
rustflags=$("$root/scripts/build-guest-sysroot.sh" "$target" "$work/rust-$arch")
(cd "$root/guest/rust" && CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=$rust_checks \
    CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=$rust_checks RUSTFLAGS="$rustflags" \
    CARGO_TARGET_DIR="$work/rust-$arch/target" \
    cargo build --locked --release -p cladefs-linux --target "$target")
cp "$work/rust-$arch/target/$target/release/libcladefs_linux.a" "$src/fs/cladefs/"
# The kernel's memcpy and friends for Rust too, not compiler_builtins' generic ones: those are in
# members of their own, which would otherwise define them for the whole kernel (arm64's own are
# weak), copying a byte at a time where the kernel's copy words.
string_functions='memcpy|memmove|memset|memcmp|bcmp|strlen'
archive="$src/fs/cladefs/libcladefs_linux.a"
nm -A --defined-only "$archive" 2>/dev/null |
    awk -v names="^($string_functions)\$" '$NF ~ names { split($1, f, ":"); print f[2] }' |
    sort -u | while read -r member; do ar d "$archive" "$member"; done
cp "$archive" "$out/"

# cladefs: the commit the build is of, which the filesystem shows in /proc/fs/cladefs/build
# (scripts/build-commit.sh, TASK-111.7). The build's user, host and time the release's, not this
# build's, so that the same sources build the same kernel (board 8601).
commit=$("$root/scripts/build-commit.sh")
make -C "$src" "${make_args[@]}" -j"${CLADEFS_GUEST_KERNEL_JOBS:-$(nproc)}" KBUILD_BUILD_USER=root KBUILD_BUILD_HOST=cladefs \
    KBUILD_BUILD_TIMESTAMP="$kernel_build_date" CLADEFS_BUILD_COMMIT="$commit" "$make_target"
# The kernel's own memcpy, which Rust calls, is in the kernel: not Rust's. nm's listing whole,
# before grep reads it: errexit stops at nm failing, and nothing stops nm part way.
rust_symbols=$(nm "$src/fs/cladefs/cladefs_rust.o")
if grep -E " [TtWw] ($string_functions)\$" <<< "$rust_symbols" >&2; then
    echo "cladefs_rust.o defines the string functions above, which must be the kernel's" >&2
    exit 1
fi
cp "$src/$image" "$out/"
# Strip non-allocatable DWARF .debug_* sections from x86_64 vmlinux while keeping .BTF (SHF_ALLOC
# inside .rodata), kallsyms, and PT_LOAD/PT_NOTE segments so the cached kernel stays ~40 MB.
if [[ $arch == x86_64 && -s "$out/vmlinux" ]]; then
    "$strip_bin" --strip-debug "$out/vmlinux"
fi
echo "$out/$(basename "$image")"
