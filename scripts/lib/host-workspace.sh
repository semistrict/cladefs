# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# The workspace's half of scripts/lib/host.sh: what a run does in the hotbed workspace it runs in
# (or, for a local test host, in this checkout), run from the checkout's root, the run's tree:
#
#   bash scripts/lib/host-workspace.sh prepare [--arch ARCH WHY] [--patch BASE64] DIR COMMIT
#       [LABEL=COMMIT...]
#   bash scripts/lib/host-workspace.sh note DIR ID
#   bash scripts/lib/host-workspace.sh job DIR
#   bash scripts/lib/host-workspace.sh fetch DIR
#   bash scripts/lib/host-workspace.sh clean DIR
#
# A run's directory DIR, under the checkout's target/host-runs (host.sh's host_name), holds
# results, the host script's results; work, its work directory (CLADEFS_TEST_HOST_WORK); tree, a
# worktree of COMMIT, with the patch applied, unless COMMIT is `-` and no patch is given, when the
# run's tree is the checkout itself, as hotbed synced it, which builds in place, incrementally from
# its base's build; and trees/LABEL, a worktree of each other COMMIT, which the host script builds
# in, each its target directories its own. Everything is the workspace's: nothing of a run is on
# the host (board 7780).
#
# prepare, with --arch, first checks the machine is an ARCH one (uname -m's word, such as x86_64),
# the run needing one for WHY: on another it makes nothing and ends 2, saying so. Then it makes the
# directory and the worktrees, fetching origin if a commit is not there yet, with
# vendor/firecracker checked out in each; the commit each reports its builds of in its
# source-revision.txt (scripts/build-commit.sh); and prints a line for each tree: LABEL (`.` for the
# run's own), its commit (with +changes for a checkout that changes what it tracks),
# vendor/firecracker's commit, and its path. note keeps the job's hotbed command ID, which job
# prints, for a driver that resumes the run. fetch writes a base64 of a gzipped tar of the results
# to its standard output, but for what aren't results (below). clean removes the run's directory,
# its worktrees with it.
set -euo pipefail

root=$(pwd)

# worktree DIR LABEL COMMIT makes the worktree of COMMIT at DIR, LABEL's, and prints its line.
worktree() {
    local tree=$1 label=$2 commit=$3
    if ! git cat-file -e "$commit^{commit}" 2> /dev/null; then
        git fetch -q origin
        git cat-file -e "$commit^{commit}" 2> /dev/null ||
            { echo "$commit is not on origin: push it first" >&2; return 1; }
    fi
    git worktree add -q --detach "$tree" "$commit"
    git -C "$tree" submodule update -q --init vendor/firecracker
}

# revision TREE records what TREE builds as, and prints LABEL's line for it.
revision() {
    local label=$1 tree=$2 commit
    # An older commit's tree, of before scripts/build-commit.sh, reports its commit alone.
    if [[ -x $tree/scripts/build-commit.sh ]]; then
        "$tree/scripts/build-commit.sh" "$tree" > "$tree/source-revision.txt"
    else
        git -C "$tree" rev-parse HEAD > "$tree/source-revision.txt"
    fi
    commit=$(git -C "$tree" rev-parse HEAD)
    if [[ -n $(git -C "$tree" status --porcelain --untracked-files=no) ]]; then
        commit+=+changes
    fi
    echo "$label $commit $(git -C "$tree/vendor/firecracker" rev-parse HEAD) $tree"
}

prepare() {
    local patch='' have
    if [[ ${1:-} == --arch ]]; then
        have=$(uname -m)
        if [[ $have != "$2" ]]; then
            echo "$3: this test host is $have, the run needs $2" >&2
            return 2
        fi
        shift 3
    fi
    if [[ ${1:-} == --patch ]]; then
        patch=$2
        shift 2
    fi
    local dir=$1 commit=$2 pair label tree
    shift 2
    [[ $(basename "$dir") == run-* ]] || { echo "prepare: $dir is no run's" >&2; return 2; }
    mkdir -p "$dir/results" "$dir/work" "$dir/trees"
    if [[ $commit == - && -z $patch ]]; then
        tree=$root
    else
        tree=$dir/tree
        worktree "$tree" . "${commit/#-/HEAD}"
        if [[ -n $patch ]]; then base64 --decode <<< "$patch" | git -C "$tree" apply; fi
    fi
    revision . "$tree"
    for pair in "$@"; do
        label=${pair%%=*}
        worktree "$dir/trees/$label" "$label" "${pair#*=}"
        revision "$label" "$dir/trees/$label"
    done
}

fetch() {
    # Unresults, by name and kind, as scripts/qualify.sh prunes them: the guests' root filesystems,
    # parts caches, and cells, buckets and images, by the names the cases give them, which a job
    # killed mid-case leaves, gigabytes of them (board 1868). A directory named so, perf.data or a
    # log among the results stays.
    (cd "$1/results" && find . -mindepth 1 \( -type d \( -name rootfs -o -name jail \
        -o -name '*-cache' -o -name '.cladefsd-cache.*' -o -name '*.root' -o -name '*.data' \
        -o -name '*.base' -o -name '*.bucket' -o -name '*.img' -o -name 'store-*' \
        -o -name sandbox-image -o -name extents -o -name '*-disk[0-9]*' -o -name '.cladefsd-disk*' \) \
        -o -name '*rootfs.ext4' \) -prune -o \
        -print0) | tar -czf - -C "$1/results" --null --no-recursion -T - | base64
}

clean() {
    local dir=$1 tree
    [[ $(basename "$dir") == run-* ]] || { echo "clean: $dir is no run's" >&2; return 2; }
    for tree in "$dir/tree" "$dir"/trees/*; do
        if [[ -d $tree ]]; then git worktree remove --force "$tree"; fi
    done
    rm -rf -- "$dir"
    git worktree prune
}

case ${1:-} in
    prepare) shift; prepare "$@" ;;
    note) mkdir -p "$2" && echo "$3" > "$2/job-id" ;;
    job) cat "$2/job-id" ;;
    fetch) fetch "$2" ;;
    clean) clean "$2" ;;
    *) echo "host-workspace.sh: no such action: ${1:-}" >&2; exit 2 ;;
esac
