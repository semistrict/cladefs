#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Builds the guest the integration tests boot, for this host's architecture:
# on x86-64, an upstream longterm kernel configured by guest/test-kernel.config,
# as both a bzImage and an ELF vmlinux (PVH entry); on both, an initramfs
# holding guest/init.c, libmachine's guest agent (crates/libmachine-agent,
# static for musl) at /sbin/libmachine-agent, and a static busybox with all its
# applets in /bin for the commands the agent's tests run. /etc/resolv.conf
# links to /proc/net/pnp, where the kernel writes the DNS server of its ip=
# setting. An arm64 guest's kernel is cladefs's (scripts/build-guest-kernel.sh
# arm64): the test kernel's configuration is a PC's.
#
#   crates/libmachine/scripts/build-test-guest.sh
#
# Output in the workspace's target/guest (override with LIBMACHINE_GUEST_DIR):
# bzImage and vmlinux (x86-64), initramfs.cpio. Needs make, a C compiler,
# musl-gcc, flex, bison, bc, zstd, and libelf/openssl headers, as any kernel
# build does; cargo with the host's musl target; a static busybox (BUSYBOX, or
# busybox on the PATH, e.g. Debian's busybox-static).
set -euo pipefail

crate=$(cd "$(dirname "$0")/.." && pwd)
root=$(cd "$crate/../.." && pwd)
out=${LIBMACHINE_GUEST_DIR:-$root/target/guest}
version=6.18.63
sha256=81764a545072d160748beff4d9de2e7d95aa9f16e8261110288f067a2cc61d71
tarball=linux-$version.tar.xz
src=$out/linux-$version

mkdir -p "$out"
if [[ ! -d $src ]]; then
    if [[ ! -f $out/$tarball ]]; then
        curl -fL --retry 3 -o "$out/$tarball.part" \
            "https://cdn.kernel.org/pub/linux/kernel/v6.x/$tarball"
        mv "$out/$tarball.part" "$out/$tarball"
    fi
    echo "$sha256  $out/$tarball" | sha256sum -c --quiet
    tar -C "$out" -xf "$out/$tarball"
fi

arch=$(uname -m)
case $arch in
    x86_64) karch=x86 ;;
    aarch64) karch=arm64 ;;
    *) echo "no test guest for $arch" >&2; exit 2 ;;
esac

# The x86-64 test kernel: tinyconfig, then the fragment, then a check that
# every option stuck. On arm64 the tree only gives the initramfs its
# gen_init_cpio.
if [[ $arch == x86_64 ]]; then
    make -C "$src" -s ARCH="$karch" tinyconfig
    "$src/scripts/kconfig/merge_config.sh" -m -O "$src" "$src/.config" "$crate/guest/test-kernel.config" >/dev/null
    make -C "$src" -s olddefconfig
    missing=0
    while IFS= read -r line; do
        [[ $line =~ ^CONFIG_ ]] || continue
        if ! grep -qx "$line" "$src/.config"; then
            echo "kernel config: $line did not survive the merge" >&2
            missing=1
        fi
    done < "$crate/guest/test-kernel.config"
    (( missing == 0 ))

    make -C "$src" -s -j"$(nproc)" bzImage vmlinux
    cp "$src/arch/x86/boot/bzImage" "$out/bzImage"
    cp "$src/vmlinux" "$out/vmlinux"
fi

# The initramfs: a static init, the agent and busybox, packed by the
# kernel's own gen_init_cpio so that /dev/console needs no root to create.
musl-gcc -static -Os -Wall -Werror -o "$out/init" "$crate/guest/init.c"
cargo build --manifest-path "$root/Cargo.toml" --release --quiet \
    --target "$arch-unknown-linux-musl" -p libmachine-agent --bin libmachine-agent
cp "${CARGO_TARGET_DIR:-$root/target}/$arch-unknown-linux-musl/release/libmachine-agent" "$out/libmachine-agent"
busybox=${BUSYBOX:-$(command -v busybox || true)}
if [[ -z $busybox ]] || ldd "$busybox" >/dev/null 2>&1; then
    echo "need a static busybox (set BUSYBOX, or install busybox-static)" >&2
    exit 1
fi
cp "$busybox" "$out/busybox"
cc -O2 -o "$out/gen_init_cpio" "$src/usr/gen_init_cpio.c"
{
    cat <<LIST
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
dir /proc 0555 0 0
dir /sys 0555 0 0
dir /tmp 01777 0 0
dir /etc 0755 0 0
slink /etc/resolv.conf /proc/net/pnp 0777 0 0
dir /bin 0755 0 0
dir /sbin 0755 0 0
file /init $out/init 0755 0 0
file /sbin/libmachine-agent $out/libmachine-agent 0755 0 0
file /bin/busybox $out/busybox 0755 0 0
LIST
    "$out/busybox" --list | grep -vx busybox | sort -u | while read -r applet; do
        echo "slink /bin/$applet busybox 0777 0 0"
    done
} > "$out/initramfs.list"
"$out/gen_init_cpio" "$out/initramfs.list" > "$out/initramfs.cpio"

ls -l "$out/initramfs.cpio"
[[ $arch != x86_64 ]] || ls -l "$out/bzImage" "$out/vmlinux"
