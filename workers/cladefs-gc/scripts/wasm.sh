#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Builds `crates/gc_wasm` for `wasm32-unknown-unknown` (release, --locked) and writes its
# `wasm-bindgen` output (`--target web`) into OUT (default: workers/cladefs-gc/src/wasm), what the
# Worker imports. wasm-bindgen must be on PATH at the version Cargo.lock pins its crate to: the CLI
# and the crate agree on one ABI, and any other version is refused. What scripts/test.sh's part
# worker and scripts/build-wasm.sh (on hotbed) build with.
#
#   wasm.sh [OUT]
set -euo pipefail

worker_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd "$worker_dir/../.." && pwd)"
out=${1:-$worker_dir/src/wasm}

# The version of Cargo.lock's wasm-bindgen crate: the line after its name.
want=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' \
    "$repo_root/Cargo.lock")
[[ -n $want ]] || { echo "wasm.sh: Cargo.lock pins no wasm-bindgen" >&2; exit 1; }
command -v wasm-bindgen > /dev/null ||
    { echo "wasm.sh: no wasm-bindgen on PATH; install $want (cargo install wasm-bindgen-cli --version $want --locked)" >&2; exit 1; }
have=$(wasm-bindgen --version | awk '{ print $2 }')
[[ $have == "$want" ]] ||
    { echo "wasm.sh: wasm-bindgen $have on PATH, Cargo.lock's is $want" >&2; exit 1; }

target_dir=${CARGO_TARGET_DIR:-$repo_root/target}
cargo build --locked --manifest-path "$repo_root/Cargo.toml" -p gc_wasm \
    --target wasm32-unknown-unknown --release
mkdir -p "$out"
wasm-bindgen --target web --out-dir "$out" "$target_dir/wasm32-unknown-unknown/release/gc_wasm.wasm"
