#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Run one of the repo's scripts on Linux inside a Lima VM: lima.sh SCRIPT [ARG...]
# runs scripts/SCRIPT with the arguments there. Lima mounts the host home
# read-only, so the repo is copied into the VM first, to ~/cladefs-test, by
# content, as a host's tree is (host_tree, scripts/lib/linux-host-setup.sh);
# what the script leaves behind stays in the VM.
#
# The script takes this environment's cladefs settings, CLADEFS_* and CLADEFSD_* but CLADEFS_LIMA_INSTANCE, as they
# are (a path among them names the VM's file, absolute: CLADEFS_GUEST_KERNEL a kernel
# build-guest-kernel.sh built there), and LIBCLANG_PATH, which building Firecracker's bindings
# needs where llvm-config doesn't find libclang. Its builds are of the checkout's commit
# (CLADEFS_BUILD_COMMIT, scripts/build-commit.sh), as the VM's copy has no .git to say.
set -euo pipefail

instance="${CLADEFS_LIMA_INSTANCE:-default}"
repo="$(cd "$(dirname "$0")/../.." && pwd)"
script=$(printf '%q' "${1:?script required}")
shift
args=
if (( $# > 0 )); then args=$(printf ' %q' "$@"); fi
library=$(printf '%q' "$repo/scripts/lib/linux-host-setup.sh")
source=$(printf '%q' "$repo")
export CLADEFS_BUILD_COMMIT=${CLADEFS_BUILD_COMMIT:-$("$repo/scripts/build-commit.sh" "$repo")}
settings=
for name in $(compgen -e); do
    case $name in
        CLADEFS_LIMA_INSTANCE) ;;
        CLADEFS_* | CLADEFSD_* | LIBCLANG_PATH) settings+="export $name=$(printf '%q' "${!name}"); " ;;
    esac
done

limactl shell "$instance" -- bash -lc "
    set -euo pipefail
    source $library
    work=~ repo=$source
    host_tree cladefs-test
    $settings
    ~/cladefs-test/scripts/$script$args
"
