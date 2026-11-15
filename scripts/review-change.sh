#!/usr/bin/env bash
# Copyright 2026 The fxdax Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# A second vendor's review of a change: Codex (codex exec, Apache-2.0), read-only, given the same
# instructions as Claude's verifying-reviewer agent (.claude/agents/verifying-reviewer.md), so that
# a load-bearing change has two reviews whose blind spots differ (TASK-444.9, doc-17). It prints
# the review and keeps it in OUT (default target/reviews/<commit or base>.md). `codex review` takes
# no instructions of ours with --commit, so the change is named in them instead.
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
name="" change="" task="" out=""
while (($#)); do
    case $1 in
        --commit) name=$2; change="the commit $2 (\`git show $2\`)"; shift 2 ;;
        --base) name=$2; change="what HEAD brings beyond $2 (\`git diff $2...HEAD\`, \`git log $2..HEAD\`)"; shift 2 ;;
        --uncommitted) name=uncommitted
            change="the uncommitted changes (\`git diff HEAD\`, and \`git status\`'s untracked files)"
            shift ;;
        --task) task=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        *) echo "review-change.sh: unknown argument $1" >&2; exit 2 ;;
    esac
done
if [[ -z $name ]]; then
    echo "review-change.sh: give --commit SHA, --base BRANCH or --uncommitted" >&2
    exit 2
fi
command -v codex > /dev/null || { echo "review-change.sh: codex isn't installed" >&2; exit 1; }

out=${out:-$root/target/reviews/${name//\//-}.md}
mkdir -p "$(dirname "$out")"

# The agent's instructions, its frontmatter dropped.
instructions=$(awk 'n >= 2 { print } /^---$/ { n++ }' "$agent")
instructions+=$'\n\n'"The change to review: $change."
if [[ -n $task ]]; then
    instructions+=" It serves $task."
fi

cd "$root"
printf '%s\n' "$instructions" | codex exec --sandbox read-only --ephemeral --output-last-message "$out" - \
    > "$out.log" 2>&1 || { cat "$out.log" >&2; exit 1; }
cat "$out"
echo "review-change.sh: kept in $out (Codex's run in $out.log)" >&2
