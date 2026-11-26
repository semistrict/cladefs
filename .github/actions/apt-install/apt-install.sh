#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Installs Ubuntu's packages, the arguments, on a GitHub runner, past an outage of ubuntu.com's
# names: .github/actions/apt-install, whose action.yml says why (TASK-360). Its files are the
# runner's unless the tests name others (CLADEFS_APT_MIRRORS, CLADEFS_APT_SOURCES).
set -euo pipefail

mirrors=${CLADEFS_APT_MIRRORS:-/etc/apt/apt-mirrors.txt}
sources=${CLADEFS_APT_SOURCES:-/etc/apt/sources.list.d/ubuntu.sources}

# The image changed how apt finds its archive: mirrors listed where apt never looks protect nothing.
if grep -q "mirror+file:$mirrors" "$sources"; then
    # The image's mirrors and the fallbacks, those whose names resolve, each tried in its priority's
    # order: apt's mirror method tries each mirror for every file, lookups of a name that doesn't
    # resolve timing out after 20 s each.
    fallbacks=$(printf '%s\tpriority:%s\n' \
        https://mirrors.edge.kernel.org/ubuntu/ 10 \
        https://mirrors.mit.edu/ubuntu/ 11)
    resolved=
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        host=${line#*://}
        host=${host%%/*}
        if getent ahosts "$host" > /dev/null; then
            resolved+="$line"$'\n'
        else
            echo "::warning::$host doesn't resolve: apt won't try it"
        fi
    done < <(sort -u "$mirrors" <(echo "$fallbacks"))
    if [ -z "$resolved" ]; then
        echo "::error::none of the Ubuntu mirrors' names resolve"
        exit 1
    fi
    printf '%s' "$resolved" | sudo tee "$mirrors"
elif grep -q 'ports\.ubuntu\.com' "$sources"; then
    # GitHub's ubuntu-24.04-arm runner fetches Ubuntu Ports directly from ports.ubuntu.com instead
    # of mirror+file:/etc/apt/apt-mirrors.txt. Fall back to non-ubuntu.com Ports mirrors if
    # ports.ubuntu.com doesn't resolve.
    if ! getent ahosts ports.ubuntu.com > /dev/null; then
        echo "::warning::ports.ubuntu.com doesn't resolve: falling back to Ubuntu Ports mirror"
        fallback_uri=
        for cand in \
            https://mirrors.edge.kernel.org/ubuntu-ports/ \
            https://mirrors.mit.edu/ubuntu-ports/; do
            host=${cand#*://}
            host=${host%%/*}
            if getent ahosts "$host" > /dev/null; then
                fallback_uri=$cand
                break
            fi
        done
        if [ -z "$fallback_uri" ]; then
            echo "::error::none of the Ubuntu Ports mirrors' names resolve"
            exit 1
        fi
        updated=$(sed -E "s|https?://ports\.ubuntu\.com/ubuntu-ports/?|$fallback_uri|g" "$sources")
        printf '%s\n' "$updated" | sudo tee "$sources" > /dev/null
    fi
else
    echo "::error::the runner's apt doesn't fetch through $mirrors; .github/actions/apt-install's fallback mirrors don't apply"
    exit 1
fi

# An index no mirror serves fails the update, not an install from indices stale or absent after it.
sudo apt-get update -o APT::Update::Error-Mode=any
sudo apt-get install -y "$@"
