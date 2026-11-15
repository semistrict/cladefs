#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Runs cargo in Firecracker as cladefs builds it: vendor/firecracker, whose `cladefs` feature builds
# the extent device against cladefs's crates (crates/cladefs, and crates/virtio_extent, the device
# itself, which the fork adapts), which the series names nowhere, so that it builds
# outside this checkout as upstream's does (TASK-269, its resources/cladefs/README.md). cladefs's build
# supplies the rest, in a copy of the tree, target/firecracker, which this keeps and builds in:
#
# - cladefs's crates, dependencies appended to the copy's src/vmm/Cargo.toml;
# - cladefs's lock, vendor/firecracker.lock, the copy's Cargo.lock: the series' own has none of
#   cladefs's dependencies. Cargo must find it as it is (as --locked does), unless
#   CLADEFS_FIRECRACKER_UNLOCKED is set;
# - frame pointers, from this repo's .cargo/config.toml, which Cargo reads in the copy as in any
#   directory below it (TASK-92.39).
#
# Callers pass the feature, as `--features cladefs`, where they build the extent device. The copy is
# by content, as scripts/lib/linux-host-setup.sh's host_tree is, writing only what differs, so
# that what changed is newer than what was built from it. Its build is in its build/cargo_target,
# as Firecracker's .cargo/config.toml sets.
#
# Usage: firecracker-cargo.sh CARGO_ARGS...   e.g. build --release -p firecracker --features cladefs
#        firecracker-cargo.sh --lock          updates vendor/firecracker.lock as Cargo resolves it
#        firecracker-cargo.sh --check         fails unless vendor/firecracker.lock is as Cargo resolves
#                                             it: on every host, as scripts/test.sh's scripts part
#                                             runs it (a dependency added to cladefs's crates broke
#                                             only Linux's Firecracker and guest runs, TASK-395)
#        firecracker-cargo.sh --tree          prints the copy's directory
# CLADEFS_FIRECRACKER_SRC names the tree to copy (default vendor/firecracker), and
# CLADEFS_FIRECRACKER_TREE the copy (default target/firecracker), which must be in this repo.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
src=${CLADEFS_FIRECRACKER_SRC:-$root/vendor/firecracker}
tree=${CLADEFS_FIRECRACKER_TREE:-$root/target/firecracker}
lock=$root/vendor/firecracker.lock

[[ $# -gt 0 ]] || { echo "Usage: $0 CARGO_ARGS... | --lock | --check | --tree" >&2; exit 2; }
[[ -f $src/src/vmm/Cargo.toml ]] || { echo "$src: no Firecracker (git submodule update --init vendor/firecracker)" >&2; exit 1; }
case "$tree/" in
    "$root"/*) ;;
    *) echo "CLADEFS_FIRECRACKER_TREE $tree is not in $root, whose .cargo/config.toml it needs" >&2; exit 2 ;;
esac

# put FILE TARGET writes FILE's content to TARGET if it differs: cargo rebuilds what a newer
# manifest is of.
put() {
    cmp -s "$1" "$2" || cp "$1" "$2"
}

mkdir -p "$tree"
rsync -rlpD --checksum --delete --exclude /.git --exclude /build --exclude /Cargo.lock \
    --exclude /src/vmm/Cargo.toml "$src/" "$tree/"
manifest=$(mktemp)
trap 'rm -f "$manifest"' EXIT
{
    cat "$src/src/vmm/Cargo.toml"
    printf '\n# cladefs: its crates, which the cladefs feature builds against (scripts/firecracker-cargo.sh).\n'
    printf '[dependencies.cladefs]\npath = "%s"\n' "$root/crates/cladefs"
    printf '[dependencies.virtio_extent]\npath = "%s"\n' "$root/crates/virtio_extent"
} > "$manifest"
put "$manifest" "$tree/src/vmm/Cargo.toml"
put "$lock" "$tree/Cargo.lock"
cd "$tree"

# locked fails unless the lock is as Cargo resolves the copy.
locked() {
    cargo metadata --locked --format-version 1 > /dev/null || {
        echo "vendor/firecracker.lock is not as Cargo resolves the tree: scripts/firecracker-cargo.sh --lock updates it" >&2
        exit 1
    }
}

case "$1" in
    --tree)
        echo "$tree"
        ;;
    --lock)
        cargo metadata --format-version 1 > /dev/null
        put Cargo.lock "$lock"
        ;;
    --check)
        locked
        ;;
    *)
        [[ -n ${CLADEFS_FIRECRACKER_UNLOCKED:-} ]] || locked
        exec cargo "$@"
        ;;
esac
