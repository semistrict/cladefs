#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# The project's rename from fxdax to cladefs (TASK-437), as a script, so that a branch begun
# before it can run it and then rebase onto main with its own changes renamed too:
#
#   scripts/rename-to-cladefs.sh            rename this checkout's live files and paths, and
#                                           regenerate the Go code from the renamed .proto
#   scripts/rename-to-cladefs.sh --tree     rename the files and paths of the tree in the current
#                                           directory, whatever it is (a commit of the
#                                           Firecracker fork, from git filter-branch --tree-filter)
#   scripts/rename-to-cladefs.sh --filter   rename stdin to stdout (commit messages, from
#                                           --msg-filter)
#   scripts/rename-to-cladefs.sh --backlog  rename the backlog through its CLI: the project's
#                                           name, the docs' titles and bodies, and the open
#                                           tasks' titles, descriptions, acceptance criteria and
#                                           plans (their notes are records)
#
# fxdax, FXDAX and Fxdax become cladefs, CLADEFS and Cladefs; the frame magic fxdaxob becomes
# cladefs, the same 7 bytes. What keeps its old name:
#
# - records of what was: docs/measurements, the backlog's tasks (renamed through its CLI, open
#   ones only), dead-ends/, DONE.md, vendor/firecracker.rewrites, and the Firecracker fork's old
#   branches and tags (fxdax-v1.16.1*, fxdax-pci-r1), which stay on GitHub as they were;
# - what ubu1's own setup (its ~/docs/system-setup.d steps) made under the old name, which a
#   checkout cannot rename: the scratch disk and its mount (fxdax-scratch), the work under it
#   (fxdax-host), fxjob's slices (fxdax.slice, fxdax-work.slice, fxdax-bench.slice), the resctrl
#   group (resctrl/fxdax-bench), the setup's steps (NNN-fxdax_*.sh), the tap device's unit
#   (fxdax-vmm-test-tap), the Linux hosts' work directory (/var/lib/fxdax-test), and the
#   checkouts, ubu1's and this Mac's (src/fxdax).
#
# This script keeps its own words.
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

# Perl regexes of the names kept, one a line.
keep='fxdax-v1\.16\.1[A-Za-z0-9.-]*
fxdax-pci-r1
fxdax-scratch
fxdax-host
fxdax(?:-[A-Za-z]+)?\.slice
resctrl/fxdax-bench
[0-9]+-fxdax_[A-Za-z0-9_]+\.sh
fxdax-vmm-test-tap
/var/lib/fxdax-test
\bsrc/fxdax\b
"src" / "fxdax"
(?<=was )fxdax\b
`fxdax`(?=, on Firecracker)
docs/measurements/[^\s)`'\''"<>\]]*'
root=
if [[ $mode == repo || $mode == backlog ]]; then
    root=$(git rev-parse --show-toplevel)
    cd "$root"
    # The measurements named for the old name, cited by name alone.
    while read -r name; do
        keep+=$'\n'$(perl -e 'print quotemeta shift' "$name")
    done < <(ls docs/measurements | grep -i fxdax || true)
fi
export RENAME_KEEP=$keep

# Perl that renames each line it is given.
# shellcheck disable=SC2016
rename='BEGIN {
    $keep = join "|", split /\n/, $ENV{RENAME_KEEP};
    %to = (fxdaxob => "cladefs", fxdax => "cladefs", FXDAX => "CLADEFS", Fxdax => "Cladefs");
}
s/($keep)|(fxdaxob|fxdax|FXDAX|Fxdax)/defined $1 ? $1 : $to{$2}/ge;'

if [[ $mode == filter ]]; then
    exec perl -pe "$rename"
fi

if [[ $mode == backlog ]]; then
    backlog config set projectName cladefs
    RENAME="$rename" python3 - << 'EOF'
import glob, json, os, subprocess

def renamed(text):
    return subprocess.run(["perl", "-pe", os.environ["RENAME"]], input=text, capture_output=True,
                          text=True, check=True).stdout

def backlog(*args):
    return subprocess.run(["backlog", *args], capture_output=True, text=True, check=True).stdout

# Docs: a title in the front matter, the body after it.
for path in sorted(glob.glob("backlog/docs/doc-*.md")):
    _, front, body = open(path).read().split("---\n", 2)
    meta = dict(line.split(": ", 1) for line in front.splitlines() if ": " in line)
    title = meta["title"]
    if renamed(title) != title or renamed(body) != body:
        backlog("doc", "update", meta["id"], "--title", renamed(title), "--content", renamed(body))

# Open tasks, but the rename's own, which names both.
listed = json.loads(backlog("task", "list", "--exclude-status", "Done", "--json"))["tasks"]
for brief in (t for t in listed if t["id"] != "TASK-437"):
    task = json.loads(backlog("task", "view", brief["id"], "--json"))["task"]
    args = []
    for field, flag in (("title", "-t"), ("description", "-d"), ("implementationPlan", "--plan")):
        if task.get(field) and renamed(task[field]) != task[field]:
            args += [flag, renamed(task[field])]
    criteria = task.get("acceptanceCriteria") or []
    if any(renamed(c["text"]) != c["text"] for c in criteria):
        for c in criteria:
            args += ["--acceptance-criteria", renamed(c["text"])]
    if not args:
        continue
    backlog("task", "edit", task["id"], *args)
    checked = [str(c["index"]) for c in criteria if c["checked"]]
    if criteria and "--acceptance-criteria" in args and checked:
        backlog("task", "edit", task["id"], *[a for i in checked for a in ("--check-ac", i)])
    print(task["id"], "renamed")
EOF
    exit
fi

# The files renamed: in the repo, its live ones.
paths=(.)
if [[ $mode == repo ]]; then
    paths+=(':(exclude)backlog' ':(exclude)docs/measurements' ':(exclude)dead-ends' ':(exclude)DONE.md'
        ':(exclude)vendor/firecracker.rewrites' ':(exclude)*.pb.go' ':(exclude)scripts/rename-to-cladefs.sh')
fi
# The text files among them that say the old name, NUL-separated; none is no failure.
saying() { git grep -lzIi fxdax -- "${paths[@]}" || [[ $? == 1 ]]; }

# Contents first.
saying | xargs -0 -r perl -pi -e "$rename"

# Then paths, each file of a path that says it moved, as in the repo git tracks the move.
git ls-files -z -- "${paths[@]}" | perl -0 -ne 'print if /fxdax/i' | while IFS= read -r -d '' from; do
    to=$(printf '%s' "$from" | perl -pe "$rename")
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
    # The records' copyrights, the same authors' under the new name (scripts/license-headers.py).
    { git grep -lz 'The fxdax Authors' -- 'docs/measurements/*.sh' 'docs/measurements/*.py' ||
        [[ $? == 1 ]]; } | xargs -0 -r perl -pi -e 's/The fxdax Authors/The cladefs Authors/'
    # The Go code, generated from the renamed .proto: its descriptors hold the package's name,
    # length-prefixed, which a rename of the text would break.
    git ls-files -z -- '*.pb.go' | xargs -0 -r git rm -q --
    go/generate.sh
    git add -A go
    left=$({ git grep -cIi fxdax -- "${paths[@]}" || [[ $? == 1 ]]; } | awk -F: '{n += $NF} END {print n + 0}')
    echo "Renamed. Live files still saying fxdax (kept names): $left"
fi
