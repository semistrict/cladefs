#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Print the path of cladefs's x86-64 guest kernel of KIND (release by default; debug, kasan or
# rust-checks: scripts/lib/linux-host-setup.sh's host_kernel), built from this tree's sources: the
# one built from them before, kept in the checkout's target/guest-kernels
# (CLADEFS_TEST_HOST_KERNELS), or built now in the cache's own work directory, only what changed
# since the last build (board 7427). In a hotbed workspace the cache is the workspace's, which
# its base's setup step fills for main, so that a run of main's kernel builds none. The build's
# log goes to CLADEFS_GUEST_KERNEL_LOG (default: standard error).
#
#   scripts/guest-kernel.sh [KIND]
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
kind=${1:-release}
[[ $kind != coverage ]] || { echo "a coverage kernel builds in its run's own work directory" >&2; exit 2; }
results=$(mktemp -d)
trap 'rm -rf "$results"' EXIT
work=$results
export CLADEFS_TEST_HOST_KERNELS=${CLADEFS_TEST_HOST_KERNELS:-$repo/target/guest-kernels}
source "$repo/scripts/lib/linux-host-setup.sh"
host_kernel_log=${CLADEFS_GUEST_KERNEL_LOG:-/dev/stderr}
host_kernel "$kind"
echo "$host_kernel_image"
