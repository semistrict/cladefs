# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# A run's bucket in memory, for scripts/test-e2e.sh, which sources this: a directory of ROOT's
# (/dev/shm), cladefs-e2e-bucket.XXXXXX, which the run's EXIT trap removes. A run killed outright
# (SIGKILL: a runner's timeout, fxjob's memory limit, a dropped ssh) runs no trap, and its bucket
# stayed, memory held for good: gigabytes of them, which kept fxjob's slice past its high mark
# (board 1764). So each bucket has an owner, .owner, which its run holds locked (flock) for as long
# as it, or anything it started, lives; the next run's e2e_bucket removes every bucket whose owner
# is gone. A bucket with no .owner, of a tree from before, it leaves.

# e2e_bucket ROOT makes this run's bucket under ROOT, locked as its own, in e2e_bucket_dir, having
# first removed those of runs that are gone. One run at a time makes or sweeps (ROOT's
# cladefs-e2e-bucket.lock), so a bucket made, its owner not yet locked, is never taken for one
# whose run is gone. Its turn is fd 7, its owner fd 6, for good: fixed, for macOS's bash 3.2, whose
# tests source this too.
e2e_bucket() {
    local root=$1 lock=$1/cladefs-e2e-bucket.lock old
    [[ -e $lock ]] || : > "$lock" || return
    exec 7< "$lock" || return
    flock 7 || return
    for old in "$root"/cladefs-e2e-bucket.*; do
        [[ -d $old && -f $old/.owner ]] || continue
        if flock -n "$old/.owner" true; then
            echo "Removing $old, the bucket of a run that is gone." >&2
            rm -r -- "$old" || echo "Removing $old failed: whose is it?" >&2
        fi
    done
    e2e_bucket_dir=$(mktemp -d "$root/cladefs-e2e-bucket.XXXXXX") || return
    # Held by this shell and inherited by what it starts, the daemon among them: the bucket is in
    # use while any of them runs.
    exec 6> "$e2e_bucket_dir/.owner" || return
    flock -n 6 || return
    exec 7<&-
}
