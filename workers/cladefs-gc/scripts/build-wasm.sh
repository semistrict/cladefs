#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Rebuilds `crates/gc_wasm` for `wasm32-unknown-unknown` via `hotbed` and copies the
# `wasm-bindgen` output into `workers/cladefs-gc/src/wasm/` (optimized with `wasm-opt` if present).

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
worker_dir="$(cd "$script_dir/.." && pwd)"
repo_root="$(cd "$worker_dir/../.." && pwd)"
wbg_version="0.2.129"

cd "$repo_root"
hotbed run --kind build --wait 30m -- sh -c "set -euo pipefail
rustup target add wasm32-unknown-unknown
cargo build --locked -p gc_wasm --target wasm32-unknown-unknown --release
if [ ! -x /tmp/wbg/wasm-bindgen ]; then
  mkdir -p /tmp/wbg
  arch=\$(uname -m)
  case \"\$arch\" in
    x86_64) target=x86_64-unknown-linux-musl ;;
    aarch64) target=aarch64-unknown-linux-gnu ;;
    *) echo \"unsupported arch: \$arch\" >&2; exit 1 ;;
  esac
  curl -fsSL \"https://github.com/wasm-bindgen/wasm-bindgen/releases/download/${wbg_version}/wasm-bindgen-${wbg_version}-\${target}.tar.gz\" | tar -xz --strip-components=1 -C /tmp/wbg
fi
rm -rf target/gc_wasm_pkg && mkdir -p target/gc_wasm_pkg
/tmp/wbg/wasm-bindgen --target web --out-dir target/gc_wasm_pkg target/wasm32-unknown-unknown/release/gc_wasm.wasm
"

mkdir -p "$worker_dir/src/wasm"
# All of the output, waited for: an attached run prints 32K of it by default, which cut the module
# short (board 14404).
if ! hotbed run --no-sync --wait 10m --max-output 0 -- sh -c 'tar czf - -C target/gc_wasm_pkg . | base64' |
  base64 -d | tar xzf - -C "$worker_dir/src/wasm"; then
  echo "build-wasm.sh: the module fetched from hotbed is cut short or corrupt" >&2
  exit 1
fi

if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Os --enable-bulk-memory --enable-sign-ext --enable-mutable-globals --enable-nontrapping-float-to-int \
    "$worker_dir/src/wasm/gc_wasm_bg.wasm" -o "$worker_dir/src/wasm/gc_wasm_bg.wasm"
fi

ls -lh "$worker_dir/src/wasm"
