#!/bin/bash
# SPDX-License-Identifier: GPL-2.0 OR BSD-3-Clause
# Copyright 2026 The fxdax Authors.
#
# The digest of what a traced run of tests/traced is of (trace.sh, TASK-444.1): dax.c and what is
# built with it, fs/dax.c's patches, this directory's fake kernel, model of Fxfs, shim and the
# test, and the patches a run put in, if any (guest/mutants). scripts/check-tla.sh compares a
# trace's with the tree's: a trace of other sources says nothing of these.
#
#   trace-sources.sh [PATCH...]

set -euo pipefail
cd "$(dirname "$0")/../.."

{
    git ls-files -- guest/linux/fs/fxfs guest/linux/patches guest/sim/Makefile guest/sim/kernel \
        guest/sim/fxfs guest/sim/shim guest/sim/tests/traced.c | LC_ALL=C sort |
        while read -r path; do
            echo "$path"
            cat "$path"
        done
    for patch in "$@"; do
        echo "$(basename "$patch")"
        cat "$patch"
    done
} | if command -v sha256sum > /dev/null; then sha256sum; else shasum -a 256; fi | cut -d' ' -f1
