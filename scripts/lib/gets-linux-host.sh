#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Measure ranged GETs from object storage (cladefs-gets) on an x86-64
# Linux host, as root, into RESULTS/gets.md (scripts/gets-host.sh).
#
# CLADEFS_TEST_HOST_PHASE (linux-host-setup.sh's host_phase) is what of it runs:
# `build` builds cladefs-gets, as work; `measure` runs it, building nothing;
# unset, both (scripts/gets-host.sh runs the two as jobs of their own, TASK-368).
#
# Usage: gets-linux-host.sh SOURCE RESULTS URL OBJECT_MIB SECS
set -euo pipefail
repo=$(cd "${1:?source required}" && pwd)
results=${2:?results directory required}
url=${3:?the URL of a bucket required}
mib=${4:?the size of the object required}
secs=${5:?the seconds each measurement takes required}
source "$(dirname "${BASH_SOURCE[0]}")/linux-host-setup.sh"
host_setup
# The benchmark's tree, whose build of cladefs this shares.
host_tree bench-tree
gets=$repo/target/release/cladefs-gets
if [[ $host_phase != measure ]]; then
    host_cargo build --release --manifest-path "$repo/Cargo.toml" -p cladefs --bin cladefs-gets
    if [[ $host_phase == build ]]; then exit 0; fi
fi
[[ -x $gets ]] || { echo "$gets: not built: CLADEFS_TEST_HOST_PHASE=build builds it" >&2; exit 2; }
host_record > "$results/host.txt"
"$gets" "$url" "$mib" "$secs" > "$results/gets.md"
