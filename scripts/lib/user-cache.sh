# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# user_cache [NAME] prints the scripts' cache of downloads (XDG_CACHE_HOME's, or ~/.cache's,
# cladefs), or its directory NAME, made if it isn't. A root run under sudo with the user's HOME
# kept, as the test host's jobs run (scripts/lib/host.sh's host_command), gives what it made of
# that path, in the invoking user's home, to that user (SUDO_UID): made root's, ~/.cache/cladefs
# refused the user's later runs their downloads, `mkdir: Permission denied` with no path
# (TASK-643). Fails naming the directory it couldn't make.
user_cache() {
    local dir=${XDG_CACHE_HOME:-$HOME/.cache}/cladefs${1:+/$1} path
    mkdir -p "$dir" 2> /dev/null || { echo "user_cache: couldn't make $dir" >&2; return 1; }
    if (( EUID == 0 )) && [[ -n ${SUDO_UID:-} && $(stat -c %u "$HOME") == "$SUDO_UID" ]]; then
        # Each directory of the path inside the user's home that is root's: those made here.
        path=$dir
        while [[ $path == "$HOME"/* ]]; do
            if [[ $(stat -c %u "$path") == 0 ]]; then
                chown "$SUDO_UID:${SUDO_GID:-$SUDO_UID}" "$path" ||
                    { echo "user_cache: couldn't give $path to user $SUDO_UID" >&2; return 1; }
            fi
            path=${path%/*}
        done
    fi
    echo "$dir"
}
