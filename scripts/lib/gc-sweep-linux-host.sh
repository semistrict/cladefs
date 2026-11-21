#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Measure cladefs-gc's sweeps over a bucket of files of many cells (scripts/gc-sweep.sh) on a Linux
# host (x86-64 or arm64), as root, into RESULTS (scripts/gc-sweep-host.sh), its buckets in the work
# directory: this tree's cladefs-gc, or, given SIDE=TREEs, each TREE's, by this tree's scripts
# (TASK-395.17). CLADEFS_TEST_HOST_PHASE (linux-host-setup.sh's host_phase) is what of it runs:
# `build` builds the binaries and fills the buckets, as work (gc-sweep.sh --prepare); `measure`
# sweeps them, building nothing (--prepared); unset, both (scripts/gc-sweep-host.sh runs the two as
# jobs of their own, TASK-368).
#
# Usage: gc-sweep-linux-host.sh SOURCE RESULTS [SIDE=TREE...]
set -euo pipefail
repo=$(cd "${1:?source required}" && pwd)
results=${2:?results directory required}
shift 2
source "$(dirname "${BASH_SOURCE[0]}")/linux-host-setup.sh"
host_setup
host_record > "$results/host.txt"
case $host_phase in
    build) phase=(--prepare) ;;
    measure) phase=(--prepared) ;;
    all) phase=() ;;
esac
"$repo/scripts/gc-sweep.sh" ${phase[@]+"${phase[@]}"} "$work/gc-sweep" "$results" "$@"
