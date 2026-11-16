#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# The extent service's names and Firecracker's extent API named for cladefs (TASK-501), as a
# script, so that a branch begun before it can run it and then rebase onto main with its own
# changes renamed too:
#
#   scripts/rename-to-cladefs-api.sh            rename this checkout's live files and paths, and
#                                        regenerate the Go code from the renamed .proto
#   scripts/rename-to-cladefs-api.sh --tree     rename the files and paths of the tree in the current
#                                        directory, whatever it is (a commit of the Firecracker
#                                        fork, from git filter-branch --tree-filter)
#   scripts/rename-to-cladefs-api.sh --filter   rename stdin to stdout (commit messages, from
#                                        --msg-filter)
#   scripts/rename-to-cladefs-api.sh --backlog  rename the backlog through its CLI: the docs, and the
#                                        open tasks' titles, descriptions, acceptance criteria and
#                                        plans (scripts/lib/rename_backlog.py)
#
# extentd, Extentd and EXTENTD become cladefsd, Cladefsd and CLADEFSD, wherever they are: the
# gRPC service cladefs.cladefsd.v1.Cladefsd, its Go package go/cladefsd/v1, the Rust module
# cladefs::cladefsd, Firecracker's cladefsd_socket, the files a daemon or a run writes
# (cladefsd.sock, cladefsd.log, cladefsd-stats.txt) and the metrics a case reports. Thread names
# stay within Linux's 15 bytes, which perf and bpftrace see: "extentd request", "control",
# "metrics" and "signals" become "cladefsd reqs", "ctl", "metric" and "signal".
#
# Firecracker's configuration of the device, its API, says cladefs: PUT /cladefs/{id}, a VM's
# configuration file's `cladefs`, PUT /snapshot/load's `cladefs_overrides`, and the types and
# modules that make them (CladefsDeviceConfig, CladefsConfigError, CladefsBuilder,
# VmmAction::InsertCladefsDevice, CladefsOverride, vmm_config::cladefs, request::cladefs), as
# upstream names `/drives`' parser and module for its API. The device keeps its own: the extent
# store device (devices/virtio/extent, its Extent, ExtentState and the snapshot's extent_devices),
# as do the domain's extent words (extent_store, ExtentId, virtio_extent).
#
# What keeps its old name: records of what was, docs/measurements, docs/reviews, the backlog's
# tasks (renamed through its CLI, open ones only), dead-ends/, DONE.md,
# vendor/firecracker.rewrites; the earlier renames' scripts, their tests and
# crates/cladefs/src/renamed.rs, which name what they renamed; and what names the old names to say
# what became of them, OPEN_QUESTIONS.md's decisions and docs/upgrading-a-host.md. This script and
# its test keep their words. A/B tooling that reads a side from before (scripts/ab-rounds.py) spells
# the old name so that this script, run again, leaves it.
set -euo pipefail

usage() { echo "usage: $0 [--tree | --filter | --backlog]" >&2; exit 2; }
mode=repo
case ${1:-} in
    "") ;;
    --tree) mode=tree ;;
    --filter) mode=filter ;;
    --backlog) mode=backlog ;;
    *) usage ;;
esac
[[ $# -le 1 ]] || usage

# Perl that renames the text it is given, a line or a whole file (-0777), as $ARGV, the file's
# path, says: a few renames are of one Firecracker file alone.
# shellcheck disable=SC2016
rename='
s/"extentd request"/"cladefsd reqs"/g; s/"extentd control"/"cladefsd ctl"/g;
s/"extentd metrics"/"cladefsd metric"/g; s/"extentd signals"/"cladefsd signal"/g;
s/extentd/cladefsd/g; s/Extentd/Cladefsd/g; s/EXTENTD/CLADEFSD/g;
s/\bExtentDeviceConfig\b/CladefsDeviceConfig/g;
s/\bExtentConfigError\b/CladefsConfigError/g;
s/\bExtentConfig\b/CladefsConfig/g;
s/\bExtentBuilder\b/CladefsBuilder/g;
s/\bInsertExtentDevice\b/InsertCladefsDevice/g;
s/\binsert_extent_device\b/insert_cladefs_device/g;
s/parse_put_extent/parse_put_cladefs/g;
s/\bExtentOverride\b/CladefsOverride/g;
s/\bextent_overrides\b/cladefs_overrides/g;
s/\bUnknownExtentDevice\b/UnknownCladefsDevice/g;
s/\bputGuestExtentByID\b/putGuestCladefsByID/g;
s/\b(vmm_config|request)::extent\b/$1::cladefs/g;
s{/extent/\{id\}}{/cladefs/{id}}g;
s/\b((?:vm_)?resources)(\s*)\.extent\b/$1$2.cladefs/g;
s/(configuration(?: file)?\x27s )`extent`/$1`cladefs`/g;
s/`extent`( in a VM\x27s\s+configuration)/`cladefs`$1/g;
s/(`cladefs_overrides` and )`extent`/$1`cladefs`/g;
s/\bVmResources::extent\b/VmResources::cladefs/g;
if ($ARGV =~ m{(^|/)src/vmm/src/resources\.rs$}) {
    s/rename = "extent"/rename = "cladefs"/g;
    s/\bextent_devices\b/cladefs_devices/g;
    s/\bpub extent: /pub cladefs: /g;
    s/^(\s*)extent: Default::default\(\)/$1cladefs: Default::default()/mg;
}
if ($ARGV =~ m{(^|/)src/vmm/src/lib\.rs$}) { s/\bextent_devices: /cladefs_devices: /g; }
if ($ARGV =~ m{(^|/)src/firecracker/src/api_server/parsed_request\.rs$}) {
    s/\(Method::Put, "extent", /(Method::Put, "cladefs", /g;
}
if ($ARGV =~ m{(^|/)(src/firecracker/src/api_server/request|src/vmm/src/vmm_config)/mod\.rs$}) {
    s/^pub mod extent;/pub mod cladefs;/mg;
}
if ($ARGV =~ m{(^|/)resources/cladefs/swagger\.yaml$}) {
    s{#/definitions/Extent"}{#/definitions/Cladefs"}g;
    s/^  Extent:$/  Cladefs:/mg;
}
if ($ARGV =~ m{(^|/)guest/tests/test_cases/src/vm\.rs$}) { s/"extent"/"cladefs"/g; }
'
# Perl that renames a path: the files of Firecracker's configuration layer, besides extentd's.
# shellcheck disable=SC2016
rename_path='s/extentd/cladefsd/g;
s{(src/vmm/src/vmm_config|src/firecracker/src/api_server/request)/extent\.rs$}{$1/cladefs.rs};'

if [[ $mode == filter ]]; then
    exec perl -0777 -pe "$rename"
fi

if [[ $mode == repo || $mode == backlog ]]; then
    cd "$(git rev-parse --show-toplevel)"
fi

if [[ $mode == backlog ]]; then
    RENAME="$rename" python3 "$(dirname "$0")/lib/rename_backlog.py" --skip TASK-501
    exit
fi

# The files renamed: in the repo, its live ones.
paths=(.)
if [[ $mode == repo ]]; then
    paths+=(':(exclude)backlog' ':(exclude)docs/measurements' ':(exclude)docs/reviews'
        ':(exclude)dead-ends' ':(exclude)DONE.md' ':(exclude)vendor/firecracker.rewrites'
        ':(exclude)*.pb.go' ':(exclude)scripts/rename-to-cladefs-api.sh'
        ':(exclude)scripts/tests/test_rename_to_cladefs_api.py' ':(exclude)scripts/rename-to-cladefsd.sh'
        ':(exclude)scripts/tests/test_rename_to_cladefsd.py'
        ':(exclude)crates/cladefs/src/renamed.rs' ':(exclude)OPEN_QUESTIONS.md'
        ':(exclude)docs/upgrading-a-host.md')
fi
# Whether rustfmt, as the tree's rustfmt.toml and toolchain have it, leaves the Rust on stdin as it
# is. Of the fork's files one at a time, from stdin, which formats no module the file declares.
formatted() {
    local text
    text=$(cat)
    [[ $(rustfmt --edition 2024 --emit stdout <<< "$text" 2> /dev/null) == "$text" ]]
}

# Contents first, of every file that says extent, as some renames span lines; none saying it is no
# failure. In a tree of the fork, the Rust that rustfmt left as it was is formatted again: a longer
# name may take a line past its width, a module's another place among its neighbours.
{ git grep -lzIi extent -- "${paths[@]}" || [[ $? == 1 ]]; } | while IFS= read -r -d '' file; do
    before=$(< "$file")
    perl -0777 -pi -e "$rename" "$file"
    if [[ $mode == tree && $file == *.rs && $(< "$file") != "$before" ]] && formatted <<< "$before"; then
        # Its warnings of the nightly options the stable toolchain the tree pins ignores, not shown.
        after=$(rustfmt --edition 2024 --emit stdout < "$file" 2> /dev/null)
        printf '%s\n' "$after" > "$file"
    fi
done

# Then paths, each file of a path that says it moved, as in the repo git tracks the move.
git ls-files -z -- "${paths[@]}" | while IFS= read -r -d '' from; do
    to=$(printf '%s' "$from" | perl -pe "$rename_path")
    [[ $to != "$from" ]] || continue
    mkdir -p "$(dirname "$to")"
    if [[ $mode == repo ]]; then
        git mv "$from" "$to"
    else
        mv "$from" "$to"
    fi
    # The directories it leaves empty.
    dir=$(dirname "$from")
    while [[ $dir != . ]] && rmdir "$dir" 2> /dev/null; do
        dir=$(dirname "$dir")
    done
done

if [[ $mode == repo ]]; then
    # The workspace's crates, as scripts/test.sh checks them, their modules where they moved.
    cargo fmt
    # The Go code, generated from the renamed .proto: its descriptors hold the package's name,
    # length-prefixed, which a rename of the text would break.
    git ls-files -z -- '*.pb.go' | xargs -0 -r git rm -qf --
    go/generate.sh
    git add -A go
    left=$({ git grep -cIE 'extentd|Extentd|EXTENTD' -- "${paths[@]}" || [[ $? == 1 ]]; } |
        awk -F: '{n += $NF} END {print n + 0}')
    echo "Renamed. Live files still saying extentd: $left"
fi
