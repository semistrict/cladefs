#!/bin/bash
# SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
# Copyright 2026 The cladefs Authors.
#
# The kernel's sources this directory builds against, in TREE, where there is no configured tree of
# the guest's kernel: the release scripts/build-guest-kernel.sh builds, by its tag and the commit
# the tag must name (scripts/lib/guest-kernel-release.sh), those of its files the Makefile's
# REAL_SOURCES and REAL_HEADERS name and what they include, with the series's patches to
# fs/dax.c. Needs nothing built, and no configuration. A tree whose tag names another commit,
# fetched now or kept from before, is refused (board 8601).
#
#   kernel-tree.sh TREE

set -euo pipefail
cd "$(dirname "$0")"

tree=${1:?usage: kernel-tree.sh TREE}
source ../../scripts/lib/guest-kernel-release.sh
if [[ -f $tree/.sim-tree-$kernel_tag ]]; then
    kernel_release_check "$tree"
    exit 0
fi
git clone -q --depth 1 --filter=blob:none --sparse --branch "$kernel_tag" \
    https://github.com/amazonlinux/linux "$tree"
kernel_release_check "$tree"
git -C "$tree" sparse-checkout set --no-cone /fs/dax.c /fs/iomap/ /include/ /lib/xarray.c \
    /lib/radix-tree.h /lib/genalloc.c
for patch in ../linux/patches/001[0-1]-*.patch; do
    git -C "$tree" apply "$(realpath "$patch")"
done
touch "$tree/.sim-tree-$kernel_tag"
