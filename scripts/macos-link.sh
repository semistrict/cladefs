#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Cargo's linker on macOS (.cargo/config.toml): links as cc does, then signs each executable it
# made, ad hoc, with the hypervisor entitlement (crates/libmachine/hvf.entitlements). A process
# makes Hypervisor.framework VMs only so signed (libmachine, board 13063); for every other binary,
# signing changes nothing (the linker signs them ad hoc anyway). Libraries go unsigned by it.
#
#   macos-link.sh CC_ARGS...
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cc "$@"
out="" library=0
while (($#)); do
    case $1 in
        -o) out=$2; shift ;;
        -dynamiclib | -bundle | -shared) library=1 ;;
    esac
    shift
done
if [[ -n $out && $library == 0 ]]; then
    # codesign says "replacing existing signature" every time: said only if it fails.
    if ! said=$(codesign --force --sign - --entitlements "$root/crates/libmachine/hvf.entitlements" "$out" 2>&1); then
        echo "$said" >&2
        exit 1
    fi
fi
