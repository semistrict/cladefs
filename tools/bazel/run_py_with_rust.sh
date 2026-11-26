#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Runs a Python test binary with Bazel's hermetic cargo and rustc prepended to PATH.
set -euo pipefail

test_bin="$1"
cargo_bin="$2"
rustc_bin="$3"
shift 3

resolve_runfile() {
    local p="$1"
    if [[ -e "$p" ]]; then
        printf '%s/%s\n' "$(cd "$(dirname "$p")" && pwd)" "$(basename "$p")"
    elif [[ "$p" == ../* && -e "$p" ]]; then
        printf '%s/%s\n' "$(cd "$(dirname "$p")" && pwd)" "$(basename "$p")"
    elif [[ "$p" == external/* && -e "../${p#external/}" ]]; then
        local rel="../${p#external/}"
        printf '%s/%s\n' "$(cd "$(dirname "$rel")" && pwd)" "$(basename "$rel")"
    else
        echo "run_py_with_rust.sh: cannot locate tool: $p (pwd=$PWD)" >&2
        exit 1
    fi
}

cargo_abs="$(resolve_runfile "$cargo_bin")"
rustc_abs="$(resolve_runfile "$rustc_bin")"
export CARGO="$cargo_abs"
export RUSTC="$rustc_abs"
export PATH="$(dirname "$cargo_abs"):$(dirname "$rustc_abs"):${PATH}"

exec "$test_bin" "$@"
