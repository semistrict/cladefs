#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# What a build guest's free memory free page reporting hands the host, at an order of its own
# (doc-30 §3, board 13454): reporting takes only free blocks of 2^ORDER pages or more, 9 (2 MiB)
# by default, so what a build leaves free in smaller blocks stays the host's RSS. Run inside a
# guest with passwordless sudo, sampling the VM process's RSS on the host beside it, by the phase
# times this prints; one run per order, each in a VM of its own.
#
#   measure-page-reporting.sh ORDER OUT CRATE
#
# Sets the order, builds CRATE into OUT (removed at the end), then drops every cache, holds 60 s,
# compacts free memory, holds 60 s: after each, free memory per order (buddyinfo).

set -euo pipefail

order=${1:?usage: measure-page-reporting.sh ORDER OUT CRATE}
out=${2:?usage: measure-page-reporting.sh ORDER OUT CRATE}
crate=${3:?usage: measure-page-reporting.sh ORDER OUT CRATE}
param=/sys/module/page_reporting/parameters/page_reporting_order

sudo -n sh -c "echo $order > $param"
echo "uname $(uname -r) page_reporting_order $(cat $param)"
[[ $(cat $param) == "$order" ]]

# Free memory per order, in MiB, summed over zones, and what of it is at the order or above.
buddy() {
    awk -v page="$(getconf PAGESIZE)" -v order="$order" '
        { for (o = 0; o + 5 <= NF; o++) free[o] += $(o + 5) * 2 ^ o * page }
        END {
            for (o = 0; o < 11; o++) { printf "o%d %.0f  ", o, free[o] / 2^20; if (o >= order) at += free[o] }
            for (o = 0; o < order; o++) below += free[o]
            printf "| reportable %.0f MiB, below the order %.0f MiB\n", at / 2^20, below / 2^20 }' \
        /proc/buddyinfo
}

phase() {
    echo "=== $1 $(date +%s.%N)"
    awk '/^(MemFree|Cached|AnonPages):/ { printf "%s %.0f MiB  ", $1, $2 / 1024 } END { print "" }' \
        /proc/meminfo
    echo "buddy $(buddy)"
}

hold() {
    echo "--- hold $1 $(date +%s.%N)"
    vmstat -t -S M 2 31
}

rm -rf "$out"
mkdir -p "$out"
phase start
start=$(date +%s.%N)
CARGO_TARGET_DIR=$out/target CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} cargo build -q -p "$crate"
echo "--- build $(awk -v s="$start" -v e="$(date +%s.%N)" 'BEGIN { printf "%.1f", e - s }') s"
phase built
sync
sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'
phase dropped
hold dropped
phase drained
sudo -n sh -c 'echo 1 > /proc/sys/vm/compact_memory'
phase compacted
hold compacted
phase compacted-drained
rm -rf "$out"
echo "=== end $(date +%s.%N)"
