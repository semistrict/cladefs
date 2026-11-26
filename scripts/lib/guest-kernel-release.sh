# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# The release of Amazon Linux's kernel the guest kernel is built on, which scripts/build-guest-kernel.sh
# and guest/sim/kernel-tree.sh source: its tag, the newest of Amazon Linux's 6.18 releases as
# Firecracker's resources/rebuild.sh picks them, and the commit the tag must name (board 8601). A
# tag can move: a tree whose tag names another commit, cloned anew or kept from before, is refused,
# never built, as this kernel runs under every tenant's VM. A new release changes both.
kernel_tag=kernel6.18-6.18.62-127.179.amzn2023
kernel_commit=b7522fb67989bd879de78486e9e253fd2f21f1dd
kernel_series=6.18
# The date of cladefs's series of commits on it, and of the kernel's build (KBUILD_BUILD_TIMESTAMP),
# so that the same sources make the same commits and the same kernel.
kernel_build_date=2026-10-17T00:00:00Z

# kernel_release_check TREE fails, saying why, unless TREE's tag names kernel_commit.
kernel_release_check() {
    local named
    named=$(git -C "$1" rev-parse --verify --quiet "$kernel_tag^{commit}") || named="no commit"
    if [[ $named != "$kernel_commit" ]]; then
        echo "$1: $kernel_tag names $named, not $kernel_commit: refused (scripts/lib/guest-kernel-release.sh)" >&2
        return 1
    fi
}
