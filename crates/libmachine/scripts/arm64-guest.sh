#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Fetches the guest libmachine's macOS tests boot (board 13063), built on an arm64 Linux node of
# hotbed's, as this tree's sources make it: cladefs's arm64 kernel (scripts/build-guest-kernel.sh
# arm64, an Image) and the test initramfs (build-test-guest.sh). A Mac builds neither: they are
# Linux's to build. Into the workspace's target/arm64-guest (LIBMACHINE_ARM64_GUEST_DIR):
# Image, initramfs.cpio. The tests find them there, or where CLADEFS_GUEST_KERNEL and
# LIBMACHINE_GUEST_DIR say.
#
#   crates/libmachine/scripts/arm64-guest.sh
set -euo pipefail

root=$(cd "$(dirname "$0")/../../.." && pwd)
out=${LIBMACHINE_ARM64_GUEST_DIR:-$root/target/arm64-guest}
mkdir -p "$out"
blob=$(mktemp)
trap 'rm -f "$blob"' EXIT
# The build's log on standard error, the two files, packed, on standard output: base64, as
# hotbed passes text. All of it, waited for: an attached run prints 32K of its output by default
# and leaves a run past its wait detached, and either cut the files short (board 14404).
(cd "$root" && hotbed run --arch arm64 ${LIBMACHINE_HOTBED_NODE:+--node "$LIBMACHINE_HOTBED_NODE"} --soft-limit 40m --wait 45m --max-output 0 -- bash -c '
    set -euo pipefail
    export CARGO_BUILD_JOBS=4 CLADEFS_GUEST_KERNEL_JOBS=4
    guest=$PWD/target/guest
    # A build cut short (the workspace out of room, board 13401) leaves truncated .cmd files
    # make cannot read: start over then.
    work=$PWD/target/guest-kernel-work
    [[ ! -e $work/failed ]] || rm -r "$work"
    mkdir -p "$work"
    touch "$work/failed"
    CLADEFS_GUEST_KERNEL_WORK=$work scripts/build-guest-kernel.sh arm64 >&2
    image=$work/out/arm64/Image
    rm "$work/failed"
    LIBMACHINE_GUEST_DIR=$guest crates/libmachine/scripts/build-test-guest.sh >&2
    cp "$image" "$guest/Image"
    echo "--- arm64 guest ---"
    tar -C "$guest" -czf - Image initramfs.cpio | base64 -w 0
    echo
' 2> >(cat >&2)) | sed -n '/^--- arm64 guest ---$/{n;p;}' > "$blob"
if ! base64 -d < "$blob" | tar -C "$out" -xzf -; then
    echo "arm64-guest.sh: the guest fetched from hotbed is cut short or corrupt" >&2
    exit 1
fi
ls -l "$out/Image" "$out/initramfs.cpio"
