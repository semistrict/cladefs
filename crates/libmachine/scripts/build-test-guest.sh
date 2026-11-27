#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Builds the test initramfs the integration tests boot: guest/init.c, libmachine's guest agent
# (crates/libmachine-agent, static for musl) at /sbin/libmachine-agent, and a static busybox with
# all its applets in /bin for the commands the tests run. /etc/resolv.conf links to /proc/net/pnp,
# where the kernel writes the DNS server of its ip= setting. The guest's kernel is cladefs's
# (scripts/build-guest-kernel.sh arm64), which crates/libmachine/scripts/arm64-guest.sh builds
# with this.
#
#   crates/libmachine/scripts/build-test-guest.sh
#
# Output in the workspace's target/guest (override with LIBMACHINE_GUEST_DIR): initramfs.cpio. Needs
# make, a C compiler, musl-gcc, cargo with the host's musl target, and a static busybox (BUSYBOX,
# or busybox on the PATH, e.g. Debian's busybox-static). An arm64 Linux host's.
set -euo pipefail

crate=$(cd "$(dirname "$0")/.." && pwd)
root=$(cd "$crate/../.." && pwd)
out=${LIBMACHINE_GUEST_DIR:-$root/target/guest}
version=6.18.63
sha256=81764a545072d160748beff4d9de2e7d95aa9f16e8261110288f067a2cc61d71
tarball=linux-$version.tar.xz
src=$out/linux-$version

arch=$(uname -m)
[[ $arch == aarch64 ]] || { echo "the test guest is arm64's: this host is $arch" >&2; exit 2; }

# The kernel's source gives the initramfs its gen_init_cpio, and nothing more.
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
