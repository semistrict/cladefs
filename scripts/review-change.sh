#!/usr/bin/env bash
# Copyright 2026 The fxdax Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# A second vendor's review of a change: Codex (codex review, Apache-2.0), read-only, given the same
# instructions as Claude's verifying-reviewer agent (.claude/agents/verifying-reviewer.md), so that
# a load-bearing change has two reviews whose blind spots differ (TASK-444.9, doc-17). It prints
# the review and keeps it in OUT (default target/reviews/<commit or base>.md).
#
#   scripts/review-change.sh --commit SHA [--task TASK-N] [--out FILE]
#   scripts/review-change.sh --base BRANCH [--task TASK-N] [--out FILE]
#   scripts/review-change.sh --uncommitted [--task TASK-N] [--out FILE]
#
# A finding counts once it has a reproducer in one of the project's oracles (TASK-444): the review
# names one; whoever acts on it writes it first.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
agent="$root/.claude/agents/verifying-reviewer.md"
scope=() task="" out=""
while (($#)); do
    case $1 in
        --commit | --base) scope=("$1" "$2"); shift 2 ;;
        --uncommitted) scope=("$1"); shift ;;
        --task) task=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        *) echo "review-change.sh: unknown argument $1" >&2; exit 2 ;;
    esac
done
if ((${#scope[@]} == 0)); then
    echo "review-change.sh: give --commit SHA, --base BRANCH or --uncommitted" >&2
    exit 2
fi
command -v codex > /dev/null || { echo "review-change.sh: codex isn't installed" >&2; exit 1; }

name=${scope[1]:-uncommitted}
out=${out:-$root/target/reviews/${name//\//-}.md}
mkdir -p "$(dirname "$out")"

# The agent's instructions, its frontmatter dropped.
instructions=$(awk 'n >= 2 { print } /^---$/ { n++ }' "$agent")
if [[ -n $task ]]; then
    instructions+=$'\n\n'"The change serves $task."
fi

cd "$root"
printf '%s\n' "$instructions" | codex review "${scope[@]}" -c sandbox_mode='"read-only"' - | tee "$out"
echo "review-change.sh: kept in $out" >&2
