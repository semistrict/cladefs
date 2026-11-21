# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# The test host's half of scripts/lib/host.sh: what a run does on the test host it runs on, sent
# there over ssh on bash's standard input (host_side) and run as the user ssh logs in as, who may
# sudo without a password:
#
#   bash -s -- prepare [--arch ARCH WHY] DIR REPO COMMIT [LABEL=COMMIT...]
#   bash -s -- launch DIR NAME BENCH MEMORY
#   bash -s -- wait DIR
#   bash -s -- log DIR
#   bash -s -- fetch DIR
#   bash -s -- cancel DIR
#   bash -s -- reap NAME
#   bash -s -- clean DIR REPO
#   bash -s -- sweep SCRATCH REPO NAME
#   bash -s -- room SCRATCH
#   bash -s -- size DIR
#
# A run's directory DIR, named for the run (run-..., host_run_name), holds tree, a worktree of
# COMMIT of the test host's clone REPO (a path in the home, or absolute), and trees/LABEL, one of
# each other COMMIT, which the host script builds in, each its target directories its own; results,
# the host script's results, and work, its work directory (CLADEFS_TEST_HOST_WORK); what the driver
# copied there (patch, guest-kernel); and job, the job's command, which the driver copied there
# too, its log, its pid, its exit status and its lock, which the job holds as long as it runs.
#
# prepare, with --arch, first checks the test host is an ARCH machine (uname -m's word, such as
# x86_64), the run needing one for WHY: on another it makes nothing and ends 2, saying so. Then it
# makes the worktrees, fetching REPO's origin if a commit is not there yet, with
# vendor/firecracker checked out in each and the patch, if any, applied to tree; the commit each
# reports its builds of in its source-revision.txt (scripts/build-commit.sh), as the host scripts'
# copies of the trees have no .git (host_tree); and prints a line for each tree, LABEL (`.` for
# tree), its commit and vendor/firecracker's. It first removes every run, passed or failed, that
# runs no job and has changed nothing (its directory, its job's status and log) for six hours
# (stale): its driver has fetched what it would, or is gone. A run that failed, or whose driver
# died, kept its tens of GB on the scratch disk for days, until the disk was full (TASK-491); a
# driver stopped for longer than that finds nothing to resume.
#
# launch runs job/command apart from the SSH session, as a dropped connection or the driver's
# machine sleeping ends nothing, under fxjob (the test host's setup), with --bench if
# BENCH is 1, with --mem MEMORY if one is given; it returns once the job holds its lock. wait waits
# for the lock, saying `waiting` once it has a connection, then `status N`, or `lost` if the job
# ended without its status, as with the test host's restart, or `lost unreadable` if its status file
# holds no number, or `none` if no job was launched. The status file is made, its room taken, as
# the job launches, and the job writes its status over it in place: a full disk at the job's end,
# which others' runs fill, no longer loses the status of a job whose tests passed (TASK-489,
# TASK-654). fetch writes a gzipped tar of the results to its standard output: but for what
# aren't results (unresults): the guests' root filesystems (rootfs, their images, and jails), some
# of whose files (setuid binaries) no one may read; parts caches; and stores, buckets and images,
# by the names the cases give them, which a job killed mid-case leaves, gigabytes of them, its
# script's own pruning never run (TASK-372). cancel ends the job, and clean removes the run's
# directory, its worktrees with it, once no job runs there; two cleans of one run at once, as a
# cancel's and its waiting driver's, take turns, the second finding nothing left (TASK-367).
#
# sweep removes, as clean does, every run of SCRATCH named for NAME, run-NAME-<time>-<mark>
# (host_run_name), whose job is not running: the earlier runs of a driver that runs under one
# name, one at a time, as the nightly does (scripts/nightly-host.sh's lock), left by drivers
# stopped before their end (TASK-489). Any other directory, another's run among them, it leaves.
# room prints the bytes free on SCRATCH's filesystem and where it is mounted; size, the bytes DIR
# holds, root's files among them.
#
# reap NAME kills what is left of the job NAME's processes, those in fxjob's scope of it, root's
# among them, which the user's systemd manager can neither stop nor kill: a job its memory limit
# killed (the kernel's OOM killer took one process, the manager failed to stop the rest) ran on as
# root, nextest and its tests, in a run's directory removed under it (TASK-344's first Linux suite
# on the test host). The job runs it once fxjob returns, before its status, and cancel after its
# signal. fxjob kills what is left of its scope itself since TASK-353; reap stays for what it can't
# reach: cancel's TERM ends the job's shell, freeing its lock, while fxjob still reaps, and a job
# whose fxjob was killed outright, or is older than that, leaves its scope to reap here.
# CLADEFS_TEST_HOST_PROC names the processes' directory (default /proc), for the tests.
set -euo pipefail

# The job's command runs as root, sudo's, with the user's Rust (scripts/lib/host.sh's
# host_command): what it leaves in the user's ~/.cargo and ~/.rustup, the registry's files and a
# toolchain it installed, and in ~/.cache/cladefs, the scripts' downloads (TASK-643), becomes the
# user's again, for the user's builds and runs.
give_back() {
    local place
    for place in "$HOME/.cargo" "$HOME/.rustup" "$HOME/.cache/cladefs"; do
        if [[ -d $place ]]; then
            sudo find "$place" ! -user "$(id -u)" -exec chown -h "$(id -u):$(id -g)" {} +
        fi
    done
}

# reap NAME kills the processes left in the scope of the job NAME, each process's cgroup naming it
# (fxjob-NAME-PID.scope), and succeeds once none is left.
reap() {
    local name=$1 proc=${CLADEFS_TEST_HOST_PROC:-/proc} pids tries
    for (( tries = 0; tries < 30; tries++ )); do
        pids=$(grep -l -- "/fxjob-$name-[0-9]*\.scope\$" "$proc"/[0-9]*/cgroup 2> /dev/null |
            sed 's|.*/\([0-9]*\)/cgroup$|\1|') || true
        if [[ -z $pids ]]; then return 0; fi
        # shellcheck disable=SC2086 # Pids, a line each.
        sudo kill -KILL $pids 2> /dev/null || true
        sleep 1
    done
    echo "reap: processes of $name outlived it: $pids" >&2
    return 1
}

# running DIR succeeds if a job holds DIR's lock: it runs.
running() {
    [[ -e $1/job/lock ]] && ! flock -n "$1/job/lock" true
}

# stale DIR succeeds for a run that runs no job and whose directory, job's status and log are all
# six hours old or more.
stale() {
    ! running "$1" && [[ -z $(find "$1" "$1/job" -maxdepth 1 -mmin -360 2> /dev/null) ]]
}

# host_lock REPO takes, on fd 8, the lock of the runs of the clone REPO: one at a time fetches,
# adds worktrees, which share the clone's objects and modules, and removes runs.
host_lock() {
    exec 8> "$(git -C "$1" rev-parse --path-format=absolute --git-common-dir)/cladefs-host.lock"
    flock 8
}

# worktree LABEL prints where the worktree LABEL of prepare's run is: `.`'s, the run's own, is
# DIR/tree.
worktree() {
    if [[ $1 == . ]]; then echo "$dir/tree"; else echo "$dir/trees/$1"; fi
}

prepare() {
    if [[ ${1:-} == --arch ]]; then
        local have
        have=$(uname -m)
        if [[ $have != "$2" ]]; then
            echo "$3: this test host is $have, the run needs $2" >&2
            return 2
        fi
        shift 3
    fi
    local dir=$1 repo=$2 pairs=(".=$3") pair label commit tree fetched=0 old
    shift 3
    pairs+=("$@")
    [[ $(basename "$dir") == run-* ]] || { echo "prepare: $dir is no run's" >&2; return 2; }
    repo=$(cd && cd "$repo" && pwd)
    mkdir -p "$dir/results" "$dir/work" "$dir/job" "$dir/trees"
    for old in "$(dirname "$dir")"/run-*; do
        if [[ $old != "$dir" && -d $old ]] && stale "$old"; then
            echo "Removing $old, which ran nothing for six hours." >&2
            clean "$old" "$repo"
        fi
    done
    host_lock "$repo"
    for pair in "${pairs[@]}"; do
        label=${pair%%=*} commit=${pair#*=}
        if ! git -C "$repo" cat-file -e "$commit^{commit}" 2> /dev/null; then
            if (( ! fetched )); then
                git -C "$repo" fetch -q origin
                fetched=1
            fi
            git -C "$repo" cat-file -e "$commit^{commit}" 2> /dev/null ||
                { echo "$commit is not on origin: push it first" >&2; return 1; }
        fi
        tree=$(worktree "$label")
        git -C "$repo" worktree add -q --detach "$tree" "$commit"
        git -C "$tree" submodule update -q --init vendor/firecracker
    done
    flock -u 8
    if [[ -f $dir/patch ]]; then
        git -C "$dir/tree" apply "$dir/patch"
    fi
    for pair in "${pairs[@]}"; do
        tree=$(worktree "${pair%%=*}")
        # An older commit's tree, of before scripts/build-commit.sh, reports its commit alone.
        if [[ -x $tree/scripts/build-commit.sh ]]; then
            "$tree/scripts/build-commit.sh" "$tree" > "$tree/source-revision.txt"
        else
            git -C "$tree" rev-parse HEAD > "$tree/source-revision.txt"
        fi
        echo "${pair%%=*} $(git -C "$tree" rev-parse HEAD) $(git -C "$tree/vendor/firecracker" rev-parse HEAD)"
    done
}

launch() {
    local dir=$1 name=$2 bench=$3 memory=$4 job=$1/job
    local fxjob=(fxjob --name "$name")
    if [[ $bench == 1 ]]; then fxjob+=(--bench); fi
    if [[ -n $memory ]]; then fxjob+=(--mem "$memory"); fi
    exec 9> "$job/lock"
    flock -n 9 || { echo "a job runs in $dir already" >&2; return 1; }
    rm -f "$job/status" "$job/pid"
    # The status's room, taken now, written over in place (1<>, no truncation) as the job ends.
    printf '%-20s\n' pending > "$job/status"
    # The lock is fd 9's, which the job's shell inherits and holds until it has written the
    # status, and nothing it runs inherits: a daemon left behind holds no run's lock.
    # shellcheck disable=SC2016 # For the job's shell.
    setsid nohup bash -c "$(declare -f give_back reap)"'
        job=$1 name=$2
        shift 2
        status=0
        "$@" 9>&- || status=$?
        reap "$name" 9>&- || status=1
        give_back 9>&- || echo "giving back ~/.cargo and ~/.rustup failed" >&2
        printf "%-20s\n" "$status" 1<> "$job/status"' \
        job "$job" "$name" "${fxjob[@]}" -- bash "$job/command" > "$job/log" 2>&1 < /dev/null &
    echo "$!" > "$job/pid"
}

wait_job() {
    local job=$1/job status
    echo waiting
    if [[ ! -e $job/pid ]]; then
        echo none
        return 0
    fi
    flock "$job/lock" true
    if [[ ! -f $job/status ]]; then
        echo lost
        return 0
    fi
    status=$(cat "$job/status" 2> /dev/null) || status=''
    status=${status//[[:space:]]/}
    if [[ $status =~ ^[0-9]+$ ]]; then
        echo "status $status"
    elif [[ $status == pending ]]; then
        echo lost
    else
        echo "lost unreadable"
    fi
}

cancel() {
    local dir=$1
    if running "$dir"; then
        # The job's process group (setsid), sudo's command among it, which sudo hands the signal;
        # then what is left of its scope, root's processes in a session of sudo's own.
        sudo kill -TERM -- "-$(< "$dir/job/pid")"
        flock "$dir/job/lock" true
    fi
    reap "$(basename "$dir")"
}

clean() {
    local dir=$1 repo
    [[ $(basename "$dir") == run-* ]] || { echo "clean: $dir is no run's" >&2; return 2; }
    repo=$(cd && cd "$2" && pwd)
    host_lock "$repo"
    if running "$dir"; then
        echo "clean: a job runs in $dir" >&2
        flock -u 8
        return 1
    fi
    if [[ -e $dir ]]; then
        sudo rm -r -- "$dir" || { flock -u 8; return 1; }
    fi
    git -C "$repo" worktree prune
    flock -u 8
}

sweep() {
    local scratch=$1 repo=$2 name=$3 dir
    [[ $name =~ ^[a-z0-9]+$ ]] || { echo "sweep: $name names no runs" >&2; return 2; }
    for dir in "$scratch/run-$name-"*; do
        [[ -d $dir && $(basename "$dir") =~ ^run-$name-[0-9]{8}T[0-9]{6}Z-[0-9a-f]{8}$ ]] ||
            continue
        if running "$dir"; then
            echo "Keeping $dir: its job runs." >&2
            continue
        fi
        echo "Removing $dir, an earlier run of $name's." >&2
        clean "$dir" "$repo"
    done
}

room() {
    local avail mount
    mkdir -p "$1"
    read -r _ _ _ avail _ mount < <(df -Pk "$1" | sed -n 2p)
    [[ $avail =~ ^[0-9]+$ ]] || { echo "room: df says no free space of $1" >&2; return 1; }
    echo "$((avail * 1024)) $mount"
}

size() {
    local kib
    kib=$(sudo du -sk "$1" | cut -f1)
    [[ $kib =~ ^[0-9]+$ ]] || { echo "size: du says no size of $1" >&2; return 1; }
    echo "$((kib * 1024))"
}

# The script's own text is this shell's standard input, which nothing it runs reads.
case ${1:-} in
    prepare) shift; prepare "$@" ;;
    launch) shift; launch "$@" ;;
    wait) wait_job "$2" ;;
    log) cat "$2/job/log" ;;
    fetch)
        # Unresults, by name and kind, as scripts/qualify.sh prunes them: a directory named so,
        # perf.data or a log among the results stays.
        (cd "$2/results" && sudo find . -mindepth 1 \( -type d \( -name rootfs -o -name jail \
            -o -name '*-cache' -o -name '.cladefsd-cache.*' -o -name '*.root' -o -name '*.data' \
            -o -name '*.base' -o -name '*.bucket' -o -name '*.img' -o -name 'store-*' \
            -o -name sandbox-image -o -name extents \) -o -name '*rootfs.ext4' \) -prune -o \
            -print0) | sudo tar -czf - -C "$2/results" --null --no-recursion -T -
        ;;
    cancel) cancel "$2" ;;
    reap) reap "$2" ;;
    clean) shift; clean "$@" ;;
    sweep) shift; sweep "$@" ;;
    room) room "$2" ;;
    size) size "$2" ;;
    *) echo "host-remote.sh: no such action: ${1:-}" >&2; exit 2 ;;
esac < /dev/null
