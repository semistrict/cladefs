#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Which of bucket_cell::Rules the madsim world's searches find, and how fast (board 14493): each
# rule, turned off, searched over BUDGET seeds of the profile that reaches it
# (crates/cladefs/tests/madsim.rs's `search_named_rule`), JOBS searches at once. Also the wall time
# of a seed of the world's other tests. Prints one line a rule, `RULE PROFILE found at seed N in S
# s` or `RULE PROFILE none of BUDGET in S s`.
#
#   scripts/madsim-rule-searches.sh [BUDGET] [JOBS]
#
# Under hotbed, as every compile (AGENTS.md): it builds the madsim target first.
set -euo pipefail

budget=${1:-2000} jobs=${2:-8}
root=$(cd "$(dirname "$0")/.." && pwd)
export RUSTFLAGS="--cfg madsim${CLADEFS_MADSIM_LLD:+ -Clink-arg=-fuse-ld=lld}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}/madsim"
cd "$root"
binary=$(cargo test --locked -p cladefs --test madsim --no-run --message-format=json 2> /dev/null |
    python3 -c 'import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if message.get("reason") == "compiler-artifact" and message.get("executable") and \
            message["target"]["name"] == "madsim":
        print(message["executable"])')
[[ -x $binary ]] || { echo "madsim-rule-searches.sh: no madsim test binary built" >&2; exit 1; }

# A seed of the other tests, three times: what a seed costs.
for seed in 1 2 3; do
    start=$(date +%s)
    env MADSIM_TEST_SEED=$seed "$binary" --skip _needs_ --skip search_named_rule \
        --test-threads 1 > /dev/null 2>&1 || echo "seed $seed of the other tests failed"
    echo "seed $seed of the other tests in $(($(date +%s) - start)) s"
done

# Each rule of bucket_cell::Rules but the fast WAL's (board 14495 brings fast WAL cells here), with
# the profile whose steps reach it.
searches="head_swap plain
epoch_keys plain
epoch_sequences plain
tokens plain
wal_follows plain
installs_own plain
collector_epochs collected
candidates_first collected
versions_kept collected
versions_carried collected
versions_acked collected
delete_tokens bases
forks_check_versions bases
forks_pending bases
collector_abandons bases
references bases
clones_refer_first bases
references_held bases
references_relisted bases
origins_kept bases
clones_record_first bases
retirements_marked bases
protect_wal_epoch collected"

search() {
    local rule=$1 profile=$2 start found
    start=$(date +%s)
    found=$(env CLADEFS_MADSIM_RULE="$rule" CLADEFS_MADSIM_PROFILE="$profile" \
        CLADEFS_MADSIM_SEEDS="$budget" "$binary" --exact search_named_rule --include-ignored \
        --nocapture --test-threads 1 2>&1 | sed -n 's/.*seed \([0-9]*\) lost: .*/\1/p' | head -1)
    local took
    took=$(($(date +%s) - start))
    if [[ -n $found ]]; then
        echo "$rule $profile found at seed $found in $took s"
    else
        echo "$rule $profile none of $budget in $took s"
    fi
}
export -f search
export budget binary
printf '%s\n' "$searches" | while read -r rule profile; do echo "$rule $profile"; done |
    xargs -P "$jobs" -L 1 bash -c 'search "$0" "$1"'
