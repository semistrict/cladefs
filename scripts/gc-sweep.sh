#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Measure cladefs-gc's sweeps over a bucket of files of many cells (TASK-395.5): how long a sweep
# takes, and the most memory it holds, by how many passes it runs at once. It builds cladefs-gc and
# cladefs-gc-fill (release, bucket_cell alone), fills WORK/bucket-SIDE once with CLADEFS_GC_TENANTS
# tenants (100) of CLADEFS_GC_CELLS cells (100) of a cell of CLADEFS_GC_SEGMENTS segments (95), the
# largest cell's CLADEFS_GC_LARGEST (950): about 1M objects in 10k cells by default, some 4 GB of
# files at most; then, for each count of CLADEFS_GC_AT_ONCE ("1 4 16 64"), sweeps a copy of it
# (hard links on Linux, clones on macOS) CLADEFS_GC_SWEEPS times (2): the first deleting and
# compacting half of every cell, the second deleting what the first compacted, and little else,
# those after skipping every cell unchanged since.
#
# CLADEFS_GC_FILL is what else the filling is told, the cells' kind (cladefs-gc-fill: `--deleted`,
# cells whose cells are all deleted; `--clones EXTENTS`, sandboxes made and deleted, clones of a
# version of their tenant's base, the tenant's first cell; TASK-443.4). With CLADEFS_GC_CELL, a cell
# as cladefs-gc names it (`1/1`, a base's), each bucket swept is then passed over at that cell
# alone, twice, each pass a process of its own: with the sweeps' options, which skips the cell if
# nothing changed it since its last pass; and with another grace period, which no pass before
# decided by, so that it goes in full, as it does once a clone was made or retired since.
#
# Each SIDE=TREE is a tree whose cladefs-gc and cladefs-gc-fill are built (in WORK/target-SIDE) and
# measured, each with a bucket its own fill made, for an A/B of commits (TASK-395.17); with none,
# this tree's, as side `this`. The sweeps go in CLADEFS_GC_ROUNDS rounds (1), each of every count on
# every side, the sides in turn first (A B, then B A), so that what drifts on the host moves all
# alike. Each sweep's metrics (cladefs-gc --metrics) and log go to
# RESULTS/sweep-SIDE-COUNT-rROUND-SWEEP.{prom,log}, the buckets' shapes to RESULTS/fill-SIDE.txt,
# a table of the sweeps to RESULTS/gc-sweep.md, and, with more than a round, each side's and
# count's median, least and most seconds a sweep; each sweep's requests by kind, the cells it
# skipped and the objects the bucket holds after it to RESULTS/gc-sweep-requests.md; and the
# passes over CLADEFS_GC_CELL to RESULTS/cell-SIDE-COUNT-rROUND-{unchanged,full}.{prom,log}, the
# cell's objects by kind as they found it to RESULTS/cell-SIDE-COUNT-rROUND-objects.txt, and a
# table of them to RESULTS/gc-sweep-cell.md: the cell's objects, the pages a cell of 1,000 keys a
# page lists them in (which cladefs-gc counts as requests, a page each, since TASK-455), the requests,
# the seconds and the most memory of each. On the test host, scripts/gc-sweep-host.sh runs it, as a
# measurement.
#
# --prepare builds the binaries and fills the buckets, and stops; --prepared sweeps what a
# --prepare of the same WORK and sides made, building nothing: a measurement's job of work, then
# its measurement, alone (scripts/lib/gc-sweep-linux-host.sh, TASK-368). Inside a measurement
# (hotbed run --bench), building is refused (scripts/lib/measurement.sh).
#
#   scripts/gc-sweep.sh [--prepare | --prepared] WORK RESULTS [SIDE=TREE...]
set -euo pipefail
# cladefs: cladefs-gc-fill encrypts the cells it fills, which cladefs-gc compacts, under the bucket KMS of a
# test master key, unless the environment names a KMS (TASK-411.28).
source "$(dirname "${BASH_SOURCE[0]}")/lib/test-kms.sh"
test_master_key
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo/scripts/lib/measurement.sh"
usage="Usage: $0 [--prepare | --prepared] WORK RESULTS [SIDE=TREE...]"
phase=all
case ${1:-} in
    --prepare) phase=prepare; shift ;;
    --prepared) phase=prepared; shift ;;
esac
(( $# >= 2 )) || { echo "$usage" >&2; exit 2; }
mkdir -p "$1" "$2"
work=$(cd "$1" && pwd)
results=$(cd "$2" && pwd)
shift 2
tenants=${CLADEFS_GC_TENANTS:-100}
cells=${CLADEFS_GC_CELLS:-100}
segments=${CLADEFS_GC_SEGMENTS:-95}
largest=${CLADEFS_GC_LARGEST:-950}
at_once=${CLADEFS_GC_AT_ONCE:-1 4 16 64}
sweeps=${CLADEFS_GC_SWEEPS:-2}
rounds=${CLADEFS_GC_ROUNDS:-1}
read -r -a kind <<< "${CLADEFS_GC_FILL:-}"
cell=${CLADEFS_GC_CELL:-}
for count in "$tenants" "$cells" "$segments" "$largest" "$rounds" "$sweeps" $at_once; do
    [[ $count =~ ^[1-9][0-9]*$ ]] || { echo "$count: not a count" >&2; exit 2; }
done
# An ID as cladefs writes it, in base 32 (crates/base32_id), and padded, as keys hold it: 13 digits.
id='[0123456789abcdefghjkmnpqrstvwxyz]{1,13}'
padded() {
    local id=$1
    while ((${#id} < 13)); do id=0$id; done
    printf %s "$id"
}
[[ -z $cell || $cell =~ ^$id/$id$ ]] ||
    { echo "CLADEFS_GC_CELL: $cell: TENANT/CELL, in base 32" >&2; exit 2; }
# Where a bucket of files keeps the cell's objects, under its directory.
cell_dir=
if [[ -n $cell ]]; then
    cell_dir="tenants/$(padded "${cell%/*}")/cells/$(padded "${cell#*/}")"
fi
case $(uname) in
    Linux) copy=(cp -al) ;;
    Darwin) copy=(cp -Rc) ;;
    *) echo "$(uname): no way to copy a bucket cheaply" >&2; exit 2 ;;
esac

# build TREE TARGET builds TREE's cladefs-gc and cladefs-gc-fill into TARGET, unless the sweeps run
# what --prepare built.
build() {
    [[ $phase != prepared ]] || return 0
    measurement_refuses "Building cladefs-gc"
    echo "== building cladefs-gc and cladefs-gc-fill of $1" >&2
    CARGO_TARGET_DIR=$2 cargo build --release -q --manifest-path "$1/Cargo.toml" -p bucket_cell \
        --bin cladefs-gc --bin cladefs-gc-fill
}

# The sides, and the directory each's binaries are in.
sides=()
bins=()
if (( $# == 0 )); then
    build "$repo" "${CARGO_TARGET_DIR:-$repo/target}"
    sides=(this)
    bins=("${CARGO_TARGET_DIR:-$repo/target}/release")
fi
for pair in "$@"; do
    side=${pair%%=*}
    [[ $pair == *=* && $side =~ ^[a-z0-9][a-z0-9-]*$ ]] ||
        { echo "$pair: SIDE=TREE, SIDE in lower-case letters, digits and -" >&2; exit 2; }
    tree=$(cd "${pair#*=}" && pwd)
    # A target of its own, as two trees sharing one would link each other's crates.
    build "$tree" "$work/target-$side"
    sides+=("$side")
    bins+=("$work/target-$side/release")
done
for bin in "${bins[@]}"; do
    [[ -x $bin/cladefs-gc && -x $bin/cladefs-gc-fill ]] ||
        { echo "$bin holds no cladefs-gc and cladefs-gc-fill: --prepare builds them" >&2; exit 2; }
done

# Each side's bucket, once for its shape.
shape="$tenants $cells $segments $largest${kind[*]:+ ${kind[*]}}"
for i in "${!sides[@]}"; do
    side=${sides[i]} bucket=$work/bucket-${sides[i]}
    if [[ ! -f $bucket.shape || $(cat "$bucket.shape") != "$shape" ]]; then
        [[ $phase != prepared ]] ||
            { echo "$bucket: not filled as $shape: --prepare fills it" >&2; exit 2; }
        rm -f "$bucket.shape"
        if [[ -e $bucket ]]; then rm -r -- "$bucket"; fi
        echo "== $side: filling $bucket: $tenants tenants of $cells cells of $segments segments" \
            "${kind[*]}" >&2
        started=$SECONDS
        "${bins[i]}/cladefs-gc-fill" "$bucket" "$tenants" "$cells" "$segments" "$largest" \
            ${kind[@]+"${kind[@]}"} > "$bucket.fill"
        sync
        echo "filled in $((SECONDS - started)) s" >&2
        echo "$shape" > "$bucket.shape"
    fi
    { echo "shape $shape"; cat "$bucket.fill"; } > "$results/fill-$side.txt"
done

if [[ $phase == prepare ]]; then exit 0; fi

# metric FILE NAME prints the value of the sample NAME (with its labels) in FILE.
metric() {
    awk -v name="$2" '$1 == name { print $2 }' "$1"
}

# The kinds of requests a sweep's passes make of a bucket, as cladefs-gc's metrics name them.
kinds=(get get-range list list-delimited create update delete-batch)

# requests FILE prints the sweep's requests of each of the kinds in FILE, as a table's cells.
requests() {
    local kind
    for kind in "${kinds[@]}"; do
        printf ' %s |' "$(metric "$1" "cladefs_gc_sweep_requests{request=\"$kind\"}")"
    done
}

# peak FILE prints the most memory the process that wrote FILE held, in MiB.
peak() {
    awk '$1 == "cladefs_gc_peak_resident_bytes" { printf "%.0f", $2 / 1048576 }' "$1"
}

# objects DIR prints how many objects the bucket of files, or the part of one, at DIR holds: its
# files, but its locks.
objects() {
    find "$1" -type f ! -name '.cladefs-lock.*' | wc -l | tr -d ' '
}

# pass_cell SWEPT BIN NAME SIDE COUNT ROUND passes over the cell of the bucket at SWEPT alone,
# unchanged since its last pass and then in full, each a process of BIN's cladefs-gc, into NAME's
# files and the cell's table.
pass_cell() {
    local swept=$1 bin=$2 name=$3 listed pass grace
    (cd "$swept/$cell_dir" && find . -type f | awk -F/ '{ print (NF > 2 ? $2 : "(the cell)") }' |
        sort | uniq -c) > "$name-objects.txt"
    listed=$(objects "$swept/$cell_dir")
    for pass in unchanged full; do
        # Another grace period than the sweeps' is another decision: no pass skips by it.
        grace=0
        if [[ $pass == full ]]; then grace=1; fi
        echo "== $4, $5 at once, round $6, cell $cell $pass" >&2
        "$bin/cladefs-gc" "file://$swept/" --cell "$cell" --grace "$grace" \
            --metrics "$name-$pass.prom" 2> "$name-$pass.log"
        {
            printf '| %s | %s | %s | %s | %s | %s | %s |' "$4" "$5" "$6" "$pass" \
                "$(metric "$name-$pass.prom" 'cladefs_gc_sweep_cells{outcome="skipped"}')" \
                "$listed" "$(( (listed + 999) / 1000 ))"
            requests "$name-$pass.prom"
            printf ' %.3f | %s |\n' "$(metric "$name-$pass.prom" cladefs_gc_sweep_seconds)" \
                "$(peak "$name-$pass.prom")"
        } >> "$cell_table"
    done
}

table=$results/gc-sweep.md
times=$results/times.tsv
by_kind=$results/gc-sweep-requests.md
cell_table=$results/gc-sweep-cell.md
heads=$(printf ' %s |' "${kinds[@]}")
{
    echo "| side | at once | round | sweep | seconds | passed | skipped |$heads objects left | peak MiB |"
    echo "|---|---|---|---|---|---|---|$(printf -- '---|%.0s' "${kinds[@]}")---|---|"
} > "$by_kind"
if [[ -n $cell ]]; then
    {
        echo "Passes over cell $cell alone, after the sweeps: unchanged since its last pass, and in full."
        echo
        echo "| side | at once | round | pass | skipped | objects | LIST pages |$heads seconds | peak MiB |"
        echo "|---|---|---|---|---|---|---|$(printf -- '---|%.0s' "${kinds[@]}")---|---|"
    } > "$cell_table"
fi
{
    echo "Bucket: $(cat "$work/bucket-${sides[0]}.fill") (tenants $tenants, cells $cells, segments $segments, largest $largest)."
    echo
    echo "| side | at once | round | sweep | seconds | cells | deleted | compacted | requests | peak MiB |"
    echo "|---|---|---|---|---|---|---|---|---|---|"
} > "$table"
: > "$times"
for (( round = 1; round <= rounds; round++ )); do
    # The sides' order this round: as given, then turned, and so on.
    order=("${!sides[@]}")
    if (( round % 2 == 0 )); then
        order=()
        for (( i = ${#sides[@]} - 1; i >= 0; i-- )); do order+=("$i"); done
    fi
    for n in $at_once; do
        for i in "${order[@]}"; do
            side=${sides[i]}
            swept=$work/sweep-$side-$n
            if [[ -e $swept ]]; then rm -r -- "$swept"; fi
            "${copy[@]}" "$work/bucket-$side" "$swept"
            for (( sweep = 1; sweep <= sweeps; sweep++ )); do
                echo "== $side, $n at once, round $round, sweep $sweep" >&2
                name=$results/sweep-$side-$n-r$round-$sweep
                "${bins[i]}/cladefs-gc" "file://$swept/" --grace 0 --at-once "$n" \
                    --metrics "$name.prom" 2> "$name.log"
                tail -n 2 "$name.log" >&2
                seconds=$(metric "$name.prom" cladefs_gc_sweep_seconds)
                requests=$(awk '$1 ~ /^cladefs_gc_sweep_requests/ { sum += $2 } END { print sum }' \
                    "$name.prom")
                printf '| %s | %s | %s | %s | %.1f | %s | %s | %s | %s | %.0f |\n' \
                    "$side" "$n" "$round" "$sweep" "$seconds" \
                    "$(metric "$name.prom" 'cladefs_gc_sweep_cells{outcome="passed"}')" \
                    "$(metric "$name.prom" 'cladefs_gc_sweep_objects{action="deleted"}')" \
                    "$(metric "$name.prom" 'cladefs_gc_sweep_objects{action="compacted"}')" \
                    "$requests" "$(peak "$name.prom")" >> "$table"
                {
                    printf '| %s | %s | %s | %s | %.1f | %s | %s |' \
                        "$side" "$n" "$round" "$sweep" "$seconds" \
                        "$(metric "$name.prom" 'cladefs_gc_sweep_cells{outcome="passed"}')" \
                        "$(metric "$name.prom" 'cladefs_gc_sweep_cells{outcome="skipped"}')"
                    requests "$name.prom"
                    printf ' %s | %s |\n' "$(objects "$swept")" "$(peak "$name.prom")"
                } >> "$by_kind"
                printf '%s\t%s\t%s\t%s\n' "$side" "$n" "$sweep" "$seconds" >> "$times"
            done
            if [[ -n $cell ]]; then
                pass_cell "$swept" "${bins[i]}" "$results/cell-$side-$n-r$round" \
                    "$side" "$n" "$round"
            fi
            rm -r -- "$swept"
        done
    done
done

# Each side's, count's and sweep's seconds over the rounds: the median, the least and the most.
if (( rounds > 1 )); then
    {
        echo
        echo "| side | at once | sweep | rounds | median s | least s | most s |"
        echo "|---|---|---|---|---|---|---|"
        for side in "${sides[@]}"; do
            for n in $at_once; do
                for (( sweep = 1; sweep <= sweeps; sweep++ )); do
                    awk -F '\t' -v side="$side" -v n="$n" -v sweep="$sweep" \
                        '$1 == side && $2 == n && $3 == sweep { print $4 }' "$times" |
                        sort -g |
                        awk -v side="$side" -v n="$n" -v sweep="$sweep" '
                            { s[NR] = $1 }
                            END {
                                m = NR % 2 ? s[(NR + 1) / 2] : (s[NR / 2] + s[NR / 2 + 1]) / 2
                                printf "| %s | %s | %s | %d | %.1f | %.1f | %.1f |\n",
                                    side, n, sweep, NR, m, s[1], s[NR]
                            }'
                done
            done
        done
    } >> "$table"
fi
cat "$table" "$by_kind" >&2
if [[ -n $cell ]]; then cat "$cell_table" >&2; fi
