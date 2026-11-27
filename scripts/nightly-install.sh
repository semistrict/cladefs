#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Install the nightly (scripts/nightly-host.sh, TASK-441) for the user running this: its clone of
# cladefs, its git identity, and its cron entry, at 02:00 local time, the test host's quiet hour
# (jobs in every hour but 02:00-04:00, 2026-11-13..15). Run again, it
# changes nothing that is right already: the entry is replaced, never added twice. The test host's
# setup calls it, as the user (docs/testing.md).
#
#   scripts/nightly-install.sh [ORIGIN]
#
# ORIGIN is what the clone fetches from: by default the origin of ~/src/fxdax, the clone the host
# runs use. The clone is CLADEFS_NIGHTLY_REPO (~/src/cladefs-nightly), made if missing; it is the
# nightly's alone, which it resets to origin's main each night. It fails, saying what is missing,
# if the nightly could not run: hotbed (which runs its builds, its measurement in a workspace on
# CLADEFS_TEST_HOST's node, and files tasks on the board), flock, hostname, or a fetch from ORIGIN
# without a prompt (cron has no agent or terminal).
# CLADEFS_NIGHTLY_CRONTAB names crontab(1) (for the tests).
set -euo pipefail
clone=${CLADEFS_NIGHTLY_REPO:-$HOME/src/cladefs-nightly}
crontab=${CLADEFS_NIGHTLY_CRONTAB:-crontab}
origin=${1:-$(git -C "$HOME/src/fxdax" remote get-url origin)}
mark="# cladefs nightly (scripts/nightly-install.sh)"

missing=()
for tool in hotbed flock hostname; do
    command -v "$tool" > /dev/null || missing+=("$tool")
done
node=${CLADEFS_TEST_HOST:-$(hostname -s)}
if ! GIT_TERMINAL_PROMPT=0 GIT_SSH_COMMAND="ssh -o BatchMode=yes" \
    git ls-remote -q "$origin" HEAD > /dev/null; then
    missing+=("a fetch from $origin without a prompt")
fi
if (( ${#missing[@]} )); then
    echo "The nightly can't run here: missing ${missing[*]}" >&2
    exit 1
fi

if [[ ! -d $clone/.git ]]; then
    mkdir -p "$(dirname "$clone")"
    git clone -q "$origin" "$clone"
fi
git -C "$clone" config user.name "cladefs nightly"
git -C "$clone" config user.email "nightly@$(hostname -s)"

# cron's PATH is /usr/bin:/bin: the tools' own directories, as this user finds them, before it.
path=
for tool in hotbed; do
    directory=$(dirname "$(command -v "$tool")")
    [[ :$path: == *:$directory:* ]] || path+=$directory:
done
path+=/usr/local/bin:/usr/bin:/bin
state=$HOME/.local/state/cladefs/nightly
mkdir -p "$state"
line="0 2 * * * PATH=$path CLADEFS_TEST_HOST=$node HOTBED_CONTROLLER=${HOTBED_CONTROLLER:-} $clone/scripts/nightly-host.sh >> $state/cron.log 2>&1 $mark"
{ "$crontab" -l 2> /dev/null | { grep -vF -- "$mark" || true; }; echo "$line"; } | "$crontab" -
echo "Installed: $line"
