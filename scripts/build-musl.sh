#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Build fully static Linux musl release binaries for cladefsd, cladefsctl, and
# cladefs-gc (matching how Firecracker builds statically against musl in scripts/test-e2e.sh),
# using mimalloc as the global allocator on musl so one binary per architecture runs across
# Ubuntu, Debian, Fedora Asahi (16 KiB pages), etc. with zero glibc coupling.
#
#   scripts/build-musl.sh [--target TARGET] [--out DIR]
#
# TARGET defaults to `$(uname -m)-unknown-linux-musl` and must be `x86_64-unknown-linux-musl` or
# `aarch64-unknown-linux-musl`.
#
# With --out DIR, copies the four verified binaries into DIR.
# Otherwise leaves them in `${CARGO_TARGET_DIR:-target}/$TARGET/release/`.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
source "$root/scripts/lib/musl.sh"
bins=(cladefsd cladefsctl cladefs-gc)

usage() {
    echo "Usage: $0 [--target x86_64-unknown-linux-musl|aarch64-unknown-linux-musl] [--out DIR]" >&2
    exit 2
}

target=""
out_dir=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            [[ $# -ge 2 ]] || usage
            target="$2"
            shift 2
            ;;
        --out)
            [[ $# -ge 2 ]] || usage
            out_dir="$2"
            shift 2
            ;;
        -h|--help)
            usage
            ;;
        *)
            usage
            ;;
    esac
done

musl_setup "$target"

# Build only the release packages (never --workspace, which unifies dev-dependencies' testing and
# tests-kms features that release builds refuse: docs/operating-a-host.md).
env "${MUSL_ENV[@]}" cargo build --release --locked \
    --manifest-path "$root/Cargo.toml" \
    --target "$MUSL_TARGET" \
    -p cladefs --bin cladefsd --bin cladefsctl \
    -p bucket_cell --bin cladefs-gc

target_dir="${CARGO_TARGET_DIR:-$root/target}/${MUSL_TARGET}/release"
paths=()
for name in "${bins[@]}"; do
    paths+=("$target_dir/$name")
done
musl_verify_static --require-debug-line "${paths[@]}"

if [[ -n $out_dir ]]; then
    mkdir -p "$out_dir"
    for name in "${bins[@]}"; do
        cp -f "$target_dir/$name" "$out_dir/$name"
    done
    echo "scripts/build-musl.sh: verified static $MUSL_TARGET binaries in $out_dir" >&2
else
    echo "scripts/build-musl.sh: verified static $MUSL_TARGET binaries in $target_dir" >&2
fi
