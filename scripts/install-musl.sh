#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Install all three static musl cladefs binaries (cladefsd, cladefsctl, cladefs-gc)
# in lockstep so cladefs-gc never drifts behind cladefsd across format bumps (hotbed question 536).
#
#   scripts/install-musl.sh [--from DIR] [--to PREFIX] [--restart]
#
# Defaults:
#   --from    ${CARGO_TARGET_DIR:-target}/$(uname -m)-unknown-linux-musl/release
#   --to      /usr/local/bin
#   --restart restart cladefsd.service after swapping binaries
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
bins=(cladefsd cladefsctl cladefs-gc)
from="${CARGO_TARGET_DIR:-$root/target}/$(uname -m)-unknown-linux-musl/release"
to="/usr/local/bin"
restart=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --from) from="$2"; shift 2 ;;
        --to) to="$2"; shift 2 ;;
        --restart) restart=1; shift ;;
        -h|--help)
            echo "Usage: $0 [--from DIR] [--to PREFIX] [--restart]" >&2
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

# Fail hard before touching anything if any binary is missing from the release set.
for bin in "${bins[@]}"; do
    if [[ ! -x "$from/$bin" ]]; then
        echo "scripts/install-musl.sh: missing $from/$bin (build all four with scripts/build-musl.sh)" >&2
        exit 1
    fi
done

sudo_cmd=()
if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
    sudo_cmd=(sudo)
fi

"${sudo_cmd[@]}" mkdir -p "$to"
for bin in "${bins[@]}"; do
    "${sudo_cmd[@]}" install -m 0755 "$from/$bin" "$to/$bin.new"
done
for bin in "${bins[@]}"; do
    "${sudo_cmd[@]}" mv -f "$to/$bin.new" "$to/$bin"
done

if [[ $restart -eq 1 ]]; then
    "${sudo_cmd[@]}" systemctl restart cladefsd
fi
echo "scripts/install-musl.sh: installed ${bins[*]} into $to ($("$to/cladefsctl" --version))" >&2
