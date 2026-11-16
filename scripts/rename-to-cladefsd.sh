#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# The extent daemon's rename to cladefsd (TASK-475.1), one daemon a host, which serves VMs' stores
# and, from TASK-475.2, the FUSE mount of cells, as a script, so that a branch begun before it can
# run it and then rebase onto main with its own changes renamed too:
#
#   scripts/rename-to-cladefsd.sh            rename this checkout's live files and paths, and
#                                            regenerate the Go code from the renamed .proto
#   scripts/rename-to-cladefsd.sh --filter   rename stdin to stdout (commit messages)
#   scripts/rename-to-cladefsd.sh --backlog  rename the backlog through its CLI: the docs, and the
#                                            open tasks' titles, descriptions, acceptance criteria
#                                            and plans (scripts/lib/rename_backlog.py)
#
# The binary cladefs-extentd becomes cladefsd, the variables CLADEFS_EXTENTD_* become CLADEFSD_*
# (and the tests' CLADEFS_EXTENTD, the daemon's socket or binary, CLADEFSD), and the metrics
# cladefs_extentd_* become cladefsd_*. What keeps its name: the extent service,
# one of cladefsd's (the Rust module cladefs::extentd, the gRPC service cladefs.extentd.v1.Extentd
# and its Go package go/extentd/v1, Firecracker's extentd_socket), and records of what was:
# docs/measurements, docs/reviews, the backlog's tasks (renamed through its CLI, open ones only),
# dead-ends/, DONE.md, vendor/firecracker.rewrites.
#
# This script, its test and crates/cladefs/src/renamed.rs, which refuses the old names, keep their
# words.
set -euo pipefail

usage() { echo "usage: $0 [--filter | --backlog]" >&2; exit 2; }
mode=repo
case ${1:-} in
    "") ;;
    --filter) mode=filter ;;
    --backlog) mode=backlog ;;
    *) usage ;;
esac
[[ $# -le 1 ]] || usage

# Perl that renames each line it is given.
rename='s/cladefs-extentd/cladefsd/g; s/CLADEFS_EXTENTD/CLADEFSD/g; s/cladefs_extentd/cladefsd/g;'

if [[ $mode == filter ]]; then
    exec perl -pe "$rename"
fi

cd "$(git rev-parse --show-toplevel)"

if [[ $mode == backlog ]]; then
    RENAME="$rename" python3 "$(dirname "$0")/lib/rename_backlog.py" --skip TASK-475
    exit
fi

paths=(. ':(exclude)backlog' ':(exclude)docs/measurements' ':(exclude)docs/reviews'
    ':(exclude)dead-ends' ':(exclude)DONE.md' ':(exclude)vendor/firecracker.rewrites'
    ':(exclude)*.pb.go' ':(exclude)scripts/rename-to-cladefsd.sh'
    ':(exclude)crates/cladefs/src/renamed.rs' ':(exclude)scripts/tests/test_rename_to_cladefsd.py')
old='cladefs-extentd|CLADEFS_EXTENTD|cladefs_extentd'

# Contents first; none saying it is no failure.
{ git grep -lzIE "$old" -- "${paths[@]}" || [[ $? == 1 ]]; } | xargs -0 -r perl -pi -e "$rename"

# Then paths, as git tracks the move.
git ls-files -z -- "${paths[@]}" | perl -0 -ne 'print if /cladefs-extentd/' |
    while IFS= read -r -d '' from; do
        git mv "$from" "$(printf '%s' "$from" | perl -pe "$rename")"
    done

# The Go code, generated from the renamed .proto, whose comments name the variables.
go/generate.sh
left=$({ git grep -cIE "$old" -- "${paths[@]}" || [[ $? == 1 ]]; } | awk -F: '{n += $NF} END {print n + 0}')
echo "Renamed. Live files still saying the old names: $left"
