#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# A second vendor's review of a change: Codex (codex exec, Apache-2.0), read-only, given the same
# instructions as Claude's verifying-reviewer agent (.claude/agents/verifying-reviewer.md), so that
# a load-bearing change has two reviews whose blind spots differ (TASK-444.9, doc-17). It prints
# the review, keeps it in OUT (default target/reviews/<commit or base>.md), and records the review
# in hotbed (`hotbed review request` -> `start` -> `findings` -> `finish`/`fail`, printing the
# cockpit link). `codex review` takes no instructions of ours with --commit, so the change is named
# in them instead.
#
#   scripts/review-change.sh --commit SHA [--task "board N"] [--review ID] [--implementer-model MODEL] [--out FILE]
#   scripts/review-change.sh --base BRANCH [--task "board N"] [--review ID] [--implementer-model MODEL] [--out FILE]
#   scripts/review-change.sh --uncommitted [--task "board N"] [--review ID] [--implementer-model MODEL] [--out FILE]
#
# --review ID takes up a review already requested; --no-hotbed records none, for offline runs only.
# A finding counts once it has a reproducer in one of the project's oracles (TASK-444): the review
# names one; whoever acts on it writes it first.
set -euo pipefail

# The checkout this script is in, which needn't be a git repository for its arguments to be read.
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
agent="$root/.claude/agents/verifying-reviewer.md"
mode="" name="" change="" task="" out="" review_id="" implementer_model="${HOTBED_MODEL:-}" use_hotbed=1
while (($#)); do
    case $1 in
        --commit) mode=commit; name=$2; change="the commit $2 (\`git show $2\`)"; shift 2 ;;
        --base) mode=base; name=$2; change="what HEAD brings beyond $2 (\`git diff $2...HEAD\`, \`git log $2..HEAD\`)"; shift 2 ;;
        --uncommitted) mode=uncommitted; name=uncommitted
            change="the uncommitted changes (\`git diff HEAD\`, and \`git status\`'s untracked files)"
            shift ;;
        --task) task=$2; shift 2 ;;
        --review) review_id=$2; shift 2 ;;
        --implementer-model) implementer_model=$2; shift 2 ;;
        --no-hotbed) use_hotbed=0; shift ;;
        --out) out=$2; shift 2 ;;
        *) echo "review-change.sh: unknown argument $1" >&2; exit 2 ;;
    esac
done
if [[ -z $name ]]; then
    echo "review-change.sh: give --commit SHA, --base BRANCH or --uncommitted" >&2
    exit 2
fi
command -v codex > /dev/null || { echo "review-change.sh: codex isn't installed" >&2; exit 1; }
if ((use_hotbed)); then
    command -v hotbed > /dev/null || {
        echo "review-change.sh: hotbed isn't installed (pass --no-hotbed only offline)" >&2
        exit 1
    }
fi

out=${out:-$root/target/reviews/${name//\//-}.md}
mkdir -p "$(dirname "$out")"

# The agent's instructions, its frontmatter dropped.
instructions=$(awk 'n >= 2 { print } /^---$/ { n++ }' "$agent")
instructions+=$'\n\n'"The change to review: $change."
if [[ -n $task ]]; then
    instructions+=" It serves $task."
fi

cd "$root"

# A field of a hotbed command's JSON output (`--json`: hotbed prints plain text by default):
# json_field data id.
json_field() {
    python3 -c 'import json, sys
d = json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[k]
print(d)' "$@"
}

if ((use_hotbed)); then
    # The caller's session, else one started for this review and ended after it.
    session=${HOTBED_SESSION:-}
    if [[ -z $session ]] && current=$(hotbed session current --json 2> /dev/null); then
        session=$(json_field data id <<< "$current")
    fi
    if [[ -z $session ]]; then
        session=$(hotbed session start --json | json_field data id)
        trap 'hotbed session end -s "$session" > /dev/null || true' EXIT
    fi
    if [[ -z $review_id ]]; then
        request=(review request -s "$session" --title "Codex review: $name${task:+ ($task)}"
            --instructions "verifying-reviewer instructions; $change${task:+, serving $task}")
        if [[ -n $implementer_model ]]; then
            request+=(--model "$implementer_model")
        fi
        case $mode in
            commit) request+=(--base "$name^" --head "$name") ;;
            base) request+=(--base "$name" --head HEAD) ;;
            uncommitted) ;;  # hotbed snapshots the uncommitted changes itself
        esac
        review_id=$(hotbed "${request[@]}" --json | json_field data id)
    fi
    # The exact model the reviewer runs, which hotbed records with it: $CODEX_MODEL, else Codex's
    # configured one.
    model=${CODEX_MODEL:-$(sed -n 's/^model = "\(.*\)"$/\1/p' "${CODEX_HOME:-$HOME/.codex}/config.toml")}
    [[ -n $model ]] || { echo "review-change.sh: no Codex model: set CODEX_MODEL" >&2; exit 2; }
    hotbed review start -s "$session" "$review_id" --model "$model" --subagent codex-exec > /dev/null
    instructions+=$'\n\n'"This is hotbed review $review_id. Your sandbox can't reach hotbed: end your report with the \`\`\`json hotbed-findings block (\`{\"findings\": []}\` if none survive)."
fi

# Fails the hotbed review with REASON, then the script.
fail() {
    if ((use_hotbed)); then
        hotbed review fail -s "$session" "$review_id" --model "$model" --subagent codex-exec --reason "$1" > /dev/null
    fi
    echo "review-change.sh: $1" >&2
    exit 1
}

printf '%s\n' "$instructions" | codex exec ${model:+-m "$model"} --sandbox read-only --ephemeral --output-last-message "$out" - \
    > "$out.log" 2>&1 || { cat "$out.log" >&2; fail "codex exec failed (its run in $out.log)"; }
cat "$out"
echo "review-change.sh: kept in $out (Codex's run in $out.log)" >&2

if ((use_hotbed)); then
    rm -f "$out.findings.json"
    summary=$(python3 "$root/scripts/review-findings.py" "$out" "$out.findings.json") \
        || fail "Codex's report carries no findings for hotbed (see $out)"
    if [[ -e $out.findings.json ]]; then
        hotbed review findings -s "$session" "$review_id" --model "$model" --subagent codex-exec --file "$out.findings.json" > /dev/null
    fi
    hotbed review finish -s "$session" "$review_id" --model "$model" --subagent codex-exec --summary "$summary" > /dev/null
    echo "review-change.sh: hotbed review $review_id, $(hotbed review link "$review_id" --json | json_field data cockpitUrl)" >&2
fi
