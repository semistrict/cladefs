#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Build the guest kernel with Fxfs in it (scripts/build-guest-kernel.sh) and run cladefs's guest
# tests of it (guest/tests) on this host, Linux with KVM, in cladefs's Firecracker: the kernel suite
# (`runner list --tag kernel`), the guest's Fxfs as its every feature is used, with a DAX window
# and without: formats, crashes, resizes, live forks, volumes side by side and shared, a lost
# daemon, imports, snapshots, suspends and migrations, the checks, the extent-store device's driver, and pjdfstest, whose rootfs podman
# builds. Elsewhere, scripts/test-host.sh runs it on a test host. The kernel is kept in
# $CLADEFS_E2E_RESULTS (default: a new temporary directory), with the tests' logs.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
results=${CLADEFS_E2E_RESULTS:-$(mktemp -d "${TMPDIR:-/tmp}/cladefs-e2e.XXXXXX")}
mkdir -p "$results"
results=$(cd "$results" && pwd)
built=$("$root/scripts/build-guest-kernel.sh" | tail -1)
cp "$built" "$results/"

CLADEFS_GUEST_KERNEL="$results/$(basename "$built")" CLADEFS_E2E_RESULTS="$results" \
    "$root/scripts/test-e2e.sh" --tag kernel
