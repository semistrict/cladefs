#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Builds the root filesystem of the Linux machine libmachine boots on an Apple Silicon Mac to run
# the tests that need Linux and KVM (the linux_test_env example): crates/libmachine/linux-test-env's
# Dockerfile over this checkout's tracked files as they are now, libmachine's init and agent and
# cladefs's arm64 guest kernel from the arm64 guest (crates/libmachine/scripts/arm64-guest.sh, or
# LIBMACHINE_GUEST_DIR), exported as one tarball, which cladefs imports as an image's single layer:
# target/linux-test-env/rootfs.tar (LIBMACHINE_LINUX_TEST_ENV). Needs docker (any engine) and
# bsdtar, macOS's tar.
#
#   crates/libmachine/scripts/linux-test-env.sh
set -euo pipefail

root=$(cd "$(dirname "$0")/../../.." && pwd)
guest=${LIBMACHINE_GUEST_DIR:-$root/target/arm64-guest}
out=${LIBMACHINE_LINUX_TEST_ENV:-$root/target/linux-test-env}
context=$out/context
for file in "$guest/Image" "$guest/initramfs.cpio"; do
    [[ -f $file ]] || { echo "no $file: run crates/libmachine/scripts/arm64-guest.sh" >&2; exit 1; }
done

rm -rf "$context"
mkdir -p "$context/work"
# The tracked files, as the working tree has them, the submodules' too.
(cd "$root" && git ls-files -z --recurse-submodules) |
    (cd "$root" && tar --null -T - -cf -) | tar -C "$context/work" -xf -
# init and the agent, out of the arm64 guest's initramfs.
bsdtar -xf "$guest/initramfs.cpio" -C "$context" init sbin/libmachine-agent
mv "$context/init" "$context/libmachine-init"
mv "$context/sbin/libmachine-agent" "$context/libmachine-agent"
rmdir "$context/sbin"
cp "$guest/Image" "$context/Image"

image=cladefs-linux-test-env
docker build --platform linux/arm64 -f "$root/crates/libmachine/linux-test-env/Dockerfile" \
    -t "$image" "$context"
container=$(docker create --platform linux/arm64 "$image")
trap 'docker rm -f "$container" >/dev/null' EXIT
docker export "$container" > "$out/rootfs.tar.part"
mv "$out/rootfs.tar.part" "$out/rootfs.tar"
rm -rf "$context"
ls -l "$out/rootfs.tar"
