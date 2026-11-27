# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Runs cladefs's host scripts (scripts/lib/*-linux-host.sh) in a hotbed workspace on a test host,
# for the drivers that source this: scripts/test-host.sh, bench-host.sh, bench-ab-host.sh,
# gets-host.sh, mutants-host.sh, fork-time-host.sh, gc-sweep-host.sh and migrate-host.sh; and
# through test-host.sh, perf-gate-host.sh, soak-host.sh, soak-workloads-host.sh,
# guest-mutants-host.py and dax-gate.sh (board 6058).
#
# A run happens in the caller's hotbed session (the one bound to this checkout, or HOTBED_SESSION;
# one started for the run, and ended with it, if there is none), in its workspace, a fork of
# main's built base that hotbed syncs this checkout into, uncommitted changes too: everything of a
# run is the workspace's, nothing the host's (board 7780). hotbed picks its node, of the
# architecture a driver needs (host_need_arch); CLADEFS_TEST_HOST is unset, or `local`, this machine
# itself, the checkout here, reached without hotbed: as a run inside a hotbed command does, such as
# the nightly's. It named a hotbed node once, which hotbed no longer takes (board 14457): a node's
# name is refused, rather than ignored.
#
# A run is a directory in the workspace's checkout, target/host-runs/RUN, RUN the run's name
# (host_name): its results, its work directory and its trees (scripts/lib/host-workspace.sh, which
# does what a run does there). Its tree is the checkout itself, building incrementally from the
# base's build, unless CLADEFS_TEST_HOST_COMMIT names a commit or CLADEFS_TEST_HOST_PATCH a patch,
# when it is a worktree of the commit (default HEAD) with the patch applied; host_start's
# LABEL=COMMIT pairs are worktrees too. Its job, the host script (host_command), is a hotbed
# command of the workspace, detached, with hotbed's time limit (CLADEFS_TEST_HOST_LIMIT, or the
# driver's limit) and memory ceiling (CLADEFS_TEST_HOST_MEM, memory.high), and with `hotbed run
# --bench`, alone on the node's measurement CPUs, if it is a measurement (host_bench=1), which
# builds first, as a command of work of its own (host_job); its builds and nextest's tests at most
# CLADEFS_TEST_HOST_JOBS at once (default 4); its guest kernels from the workspace's cache of those
# built from the same sources (host_kernel, scripts/lib/linux-host-setup.sh's, in the checkout's
# target/guest-kernels, which a base's setup step fills for main). The driver waits for the job
# (hotbed logs -f), fetches the results, and removes the run's directory, whatever happens: but for
# a job whose driver stopped waiting for it, which runs on, for the driver's `resume` to wait for
# again and fetch, or its `cancel` to end (host_actions).
#
# The sourcing script sets repo, the tree whose scripts run, before it sources this, which names
# the run; then results, the local directory the results come back to, and limit, the longest the
# job may take (such as 6h; CLADEFS_TEST_HOST_LIMIT, if set, in its place), and calls host_start,
# host_job and host_fetch. CLADEFS_TEST_HOST_RUN is the run's number, as its caller counts runs
# (such as r1040), which its name carries. CLADEFS_GUEST_KERNEL names a guest kernel in the
# workspace (a path under /work), or none: the workspace builds the tree's. A driver whose run
# boots guests calls host_need_arch x86_64 WHY before host_start, which then runs on an x86-64
# node, and refuses another, before it makes anything there: the guest kernels are x86-64's.
#
# The drivers run these functions under `||`, where errexit stops nothing, so each checks the
# status of its every step itself.

host=${CLADEFS_TEST_HOST:-}
[[ -z $host || $host == local ]] ||
    { echo "CLADEFS_TEST_HOST is unset (hotbed places the run) or local, not $host" >&2; exit 2; }
# The workspace's checkout of cladefs: hotbed's /work/REPO (CLADEFS_HOTBED_WORK names another
# /work, for the tests).
host_workspace=${CLADEFS_HOTBED_WORK:-/work}/${CLADEFS_HOTBED_REPO:-cladefs}
host_library=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# This run's mark, which its name and the names of its results end in.
host_mark=$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
[[ ${CLADEFS_TEST_HOST_RUN:-} =~ ^[a-z0-9]*$ ]] || {
    echo "CLADEFS_TEST_HOST_RUN names a run in lower-case letters and digits, such as r1040" >&2
    exit 2
}
# What a run's name looks like, for those given one (host_actions).
host_run_pattern='^run-([a-z0-9]+-)?[0-9]{8}T[0-9]{6}Z-[0-9a-f]{8}$'
# Whether the run is a measurement, which hotbed run --bench runs alone: a timed driver's always,
# any other's with CLADEFS_TEST_HOST_BENCH=1 (as scripts/perf-gate-host.sh's runs of test-host.sh).
# A measurement's host_job is two commands of the run, its builds as work, then the measurement
# alone, building nothing (host_job, TASK-368).
host_bench=${CLADEFS_TEST_HOST_BENCH:-0}
[[ $host_bench =~ ^[01]$ ]] || { echo "CLADEFS_TEST_HOST_BENCH is 0 or 1" >&2; exit 2; }
# Where the run stands: '' (nothing of it in the workspace), prepared (its directory made),
# launched (its job running, as far as this knows) or ended.
host_state=''
# A function the driver names, which host_actions' resume runs as the driver's run would once the
# results are back, with the run's status, and whose status is the resume's.
host_after=''
# The node the run is on, once host_start placed it, which its records name; the session it is in,
# and whether this driver started it; the job's hotbed command.
host_node='' host_session=${HOTBED_SESSION:-} host_session_ours=0 host_command_id=

# host_name RUN names the run RUN: its directory in the workspace is host_dir.
host_name() {
    host_run=$1
    if [[ $host == local ]]; then
        host_dir=$repo/target/host-runs/$host_run
    else
        host_dir=$host_workspace/target/host-runs/$host_run
    fi
}
# The run's name: CLADEFS_TEST_HOST_RUN, if set; when it started, to the second; and this driver's
# mark, so that two drivers started in the same second keep apart (TASK-79.86).
# shellcheck disable=SC2154 # The sourcing script sets repo.
host_name "run-${CLADEFS_TEST_HOST_RUN:+$CLADEFS_TEST_HOST_RUN-}$(date -u +%Y%m%dT%H%M%SZ)-$host_mark"

# host_label prints the test host's name for the names of results: the node's; a local test
# host's own, as its name for itself.
host_label() {
    if [[ $host == local ]]; then hostname -s; else echo "$host_node"; fi
}

# host_hotbed_said ARG... runs hotbed ARG... from the checkout, in the run's session, saying all it
# says; host_hotbed, all but where it placed a command (`hotbed: ID running in ...`), which would
# only clutter a run's logs.
host_hotbed_said() {
    (cd "$repo" && if [[ -n $host_session ]]; then export HOTBED_SESSION=$host_session; fi &&
        hotbed "$@")
}
host_hotbed() {
    local err status=0
    err=$(mktemp) || return
    host_hotbed_said "$@" 2> "$err" || status=$?
    grep -v '^hotbed: [a-z0-9]* running in ' "$err" >&2 || true
    rm -f "$err"
    return "$status"
}

# host_session_ensure takes the session hotbed binds to this checkout, or starts one for the run,
# which host_end ends.
host_session_ensure() {
    local current
    if [[ -n $host_session ]]; then return 0; fi
    if current=$(cd "$repo" && hotbed session current --json 2> /dev/null) &&
        host_session=$(python3 -c 'import json, sys; print(json.load(sys.stdin)["data"]["id"])' \
            <<< "$current" 2> /dev/null) && [[ -n $host_session ]]; then
        return 0
    fi
    current=$(cd "$repo" && hotbed session start --json) || return
    host_session=$(python3 -c '
import json, sys
data = json.load(sys.stdin)["data"]
m = data.get("memories") or {}
always = m.get("always") or []
on_demand = m.get("onDemand") or []
if always or on_demand:
    err = sys.stderr
    err.write(f"hotbed memories ({len(always)} always-on, {len(on_demand)} on-demand — `hotbed memory get SLUG`):\n")
    for e in always:
        scope = e.get("project") or "global"
        slug, summary = e["slug"], e["summary"]
        err.write(f"  [{scope}] {slug}: {summary}\n")
    for e in on_demand:
        scope = e.get("project") or "global"
        slug, summary = e["slug"], e["summary"]
        err.write(f"  [{scope}] {slug} — {summary}\n")
print(data["id"])
' <<< "$current") || return
    host_session_ours=1
}

# host_placement sets placement, the caller's, to the arguments of hotbed run that place a command
# of the run: the architecture the driver needs (host_need_arch), if any. hotbed takes no node
# (`--node` is gone, board 14457): the run's first command syncs this checkout, which places its
# workspace; every later one syncs nothing and stays in that workspace.
host_placement() {
    placement=()
    if [[ $host_arch == x86_64 ]]; then
        placement=(--arch amd64)
    elif [[ $host_arch == aarch64 ]]; then
        placement=(--arch arm64)
    fi
}

# host_shell COMMAND runs the shell command COMMAND in the run's checkout: here for a local test
# host; else as a command of the workspace, which syncs nothing more (host_start synced it).
host_shell() {
    if [[ $host == local ]]; then
        (cd "$repo" && bash -c "$1")
    else
        local placement
        host_placement
        host_hotbed run --no-sync ${placement[@]+"${placement[@]}"} --kind shell -- bash -c "$1"
    fi
}

# host_side ACTION ARG... does ACTION of scripts/lib/host-workspace.sh in the run's checkout.
host_side() {
    host_shell "bash scripts/lib/host-workspace.sh $(printf '%q ' "$@")"
}

# host_commit [REV] prints the commit REV names in repo, in full.
host_commit() {
    git -C "$repo" rev-parse --verify --quiet "$1^{commit}" ||
        { echo "$1: no commit of $repo's" >&2; return 2; }
}

# host_start [LABEL=REV...] makes the run's directory in the workspace (host-workspace.sh's
# prepare), syncing this checkout there first: its tree, and one of each REV, which the host script
# finds in host_dir/trees/LABEL; and records each in the results, source.txt, and
# LABEL/source.txt, with the run, the node and the directory there (run.txt). From here, the run's
# directory goes as the driver ends, whatever happens (host_exit).
host_start() {
    local main=- pairs=() pair sha label commit firecracker path prepared out patch=() said
    local placement arch=() err status
    if [[ -n ${CLADEFS_TEST_HOST_COMMIT:-} ]]; then
        main=$(host_commit "$CLADEFS_TEST_HOST_COMMIT") || return
    fi
    for pair in "$@"; do
        [[ $pair =~ ^[a-z0-9][a-z0-9-]*=.+$ ]] ||
            { echo "$pair: LABEL=COMMIT, LABEL in lower-case letters, digits and -" >&2; return 2; }
        sha=$(host_commit "${pair#*=}") || return
        pairs+=("${pair%%=*}=$sha")
    done
    if [[ -n ${CLADEFS_GUEST_KERNEL:-} && $host != local &&
        $CLADEFS_GUEST_KERNEL != "${CLADEFS_HOTBED_WORK:-/work}"/* ]]; then
        echo "CLADEFS_GUEST_KERNEL names a kernel in the workspace (/work/...), or none, which the" \
            "workspace builds from the tree: $CLADEFS_GUEST_KERNEL is this machine's" >&2
        return 2
    fi
    mkdir -p "$results" || return
    results=$(cd "$results" && pwd) || return
    if [[ $host != local ]]; then host_session_ensure || return; fi
    host_state=prepared
    trap host_exit EXIT
    if [[ -n ${CLADEFS_TEST_HOST_PATCH:-} ]]; then
        patch=(--patch "$(base64 < "$CLADEFS_TEST_HOST_PATCH" | tr -d '\n')") || return
    fi
    if [[ -n $host_arch ]]; then arch=(--arch "$host_arch" "$host_arch_why"); fi
    if [[ $host == local ]]; then
        prepared=$(cd "$repo" && bash scripts/lib/host-workspace.sh prepare \
            ${arch[@]+"${arch[@]}"} ${patch[@]+"${patch[@]}"} "$host_dir" "$main" \
            ${pairs[@]+"${pairs[@]}"}) || return
    else
        # The first command syncs the checkout, and says where hotbed placed it, which every later
        # command of the run keeps to: its workspace is there.
        host_placement
        err=$(mktemp) || return
        status=0
        prepared=$(host_hotbed_said run ${placement[@]+"${placement[@]}"} --kind shell -- \
            bash scripts/lib/host-workspace.sh prepare ${arch[@]+"${arch[@]}"} \
            ${patch[@]+"${patch[@]}"} "$host_dir" "$main" ${pairs[@]+"${pairs[@]}"} 2> "$err") ||
            status=$?
        said=$(sed -n 's/^hotbed: [a-z0-9]* running in [^ ]* on \([^ ]*\) .*/\1/p' "$err" | tail -1)
        grep -v '^hotbed: [a-z0-9]* running in ' "$err" >&2 || true
        rm -f "$err"
        # Placed nowhere, nothing of the run is anywhere.
        if [[ -z $said ]]; then host_state=''; else host_node=$said; fi
        (( status == 0 )) || return 2
        [[ -n $said ]] || { echo "hotbed said no node the run is on" >&2; return 1; }
    fi
    printf 'run %s\nhost %s\nsession %s\ndirectory %s\n' "$host_run" "$(host_label)" \
        "${host_session:-}" "$host_dir" > "$results/run.txt" || return
    while read -r label commit firecracker path; do
        out=$results
        if [[ $label != . ]]; then out=$results/$label; fi
        mkdir -p "$out" || return
        {
            echo "revision $commit"
            echo "firecracker $firecracker"
            if [[ $label == . && -n ${CLADEFS_TEST_HOST_PATCH:-} ]]; then
                echo "patch $(shasum -a 256 < "$CLADEFS_TEST_HOST_PATCH" | cut -d' ' -f1)" \
                    "$(basename "$CLADEFS_TEST_HOST_PATCH")"
            fi
            if [[ $label == . && -n ${CLADEFS_GUEST_KERNEL:-} ]]; then
                echo "guest kernel $CLADEFS_GUEST_KERNEL"
            fi
        } > "$out/source.txt" || return
        if [[ $label == . ]]; then host_tree=$path; fi
    done <<< "$prepared"
}

# host_need_arch ARCH WHY... has host_start run on an ARCH node (uname -m's word, such as x86_64)
# and refuse another, the run needing one for WHY (its words joined), before it makes anything
# there.
host_arch='' host_arch_why='' host_tree=''
host_need_arch() {
    host_arch=$1
    shift
    host_arch_why="$*"
}

# host_command [NAME=VALUE...] SCRIPT [ARG...] sets host_environment, the job's environment, a
# NAME=VALUE each, and host_argv, its command: the host script scripts/lib/SCRIPT of the run's
# tree, with the tree, the results and the ARGs.
#
# The script runs in this machine's environment: every CLADEFS_*, CLADEFSD_* and PJDFSTEST_*
# variable set here, but those that stay here (host_env_local); the run's own, its work directory
# and the workspace's cache of guest kernels; and each NAME=VALUE in place of the variable NAME. An
# empty value sends nothing, leaving NAME unset there, which its readers take for their default
# (an empty CLADEFS_ZSTD_LEVEL is an error).
host_command() {
    local jobs=${CLADEFS_TEST_HOST_JOBS:-4} given=() name value i j checkout
    [[ $jobs =~ ^[1-9][0-9]*$ ]] || { echo "CLADEFS_TEST_HOST_JOBS is a count" >&2; return 2; }
    checkout=$host_workspace
    if [[ $host == local ]]; then checkout=$repo; fi
    given=("CLADEFS_GUEST_KERNEL_JOBS=$jobs" "CARGO_BUILD_JOBS=$jobs" "NEXTEST_TEST_THREADS=$jobs")
    for name in ${!CLADEFS_@} ${!CLADEFSD_@} ${!PJDFSTEST_@}; do
        if ! host_env_local "$name"; then
            given+=("$name=${!name}")
        fi
    done
    given+=(CLADEFS_TEST_HOST_PROVISIONED=1 CLADEFS_TEST_HOST_IN_PLACE=1
        "CLADEFS_TEST_HOST_WORK=$host_dir/work"
        "CLADEFS_TEST_HOST_KERNELS=$checkout/target/guest-kernels")
    while (( $# > 0 )) && [[ $1 =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; do
        given+=("$1")
        shift
    done
    local script=${1:?a host script required}
    shift
    host_environment=()
    for (( i = 0; i < ${#given[@]}; i++ )); do
        name=${given[i]%%=*} value=${given[i]#*=}
        # A later value of the name replaces this one.
        for (( j = i + 1; j < ${#given[@]}; j++ )); do
            if [[ ${given[j]%%=*} == "$name" ]]; then continue 2; fi
        done
        if [[ -n $value ]]; then host_environment+=("$name=$value"); fi
    done
    host_argv=(bash "$host_tree/scripts/lib/$script" "$host_tree" "$host_dir/results" "$@")
}

# host_env_local NAME succeeds for the variables that stay here, though host_command would send
# them by their names: those this machine sets up its own builds, daemons and runs with, and which
# the host scripts, or what they run, would take as they are. Set here, they name what this machine
# built or fetched, its own daemon's sockets, address and directories, and where its own runs
# write; sent, every build, test and daemon in the workspace would use them in place of its own.
# Those the host scripts always set themselves, such as CLADEFS_FIRECRACKER and CLADEFSD
# (scripts/test-e2e.sh), need no place here. The others go: knobs a driver's caller sets for the
# run, what runs and how, and where its cell directories are and its daemons what they read from
# them (CLADEFSD_BUCKET, CLADEFS_S3_URL, CLADEFSD_PART_CACHE), or a driver's own, which nothing
# there reads.
host_env_local() {
    case $1 in
        # Docker's static binaries and BusyBox, for fxfs-guest-docker-dax:
        # scripts/test-e2e.sh fetches its own unless they are set.
        CLADEFS_DOCKER_STATIC | CLADEFS_BUSYBOX) ;;
        # Firecracker's seccomp filter and jailer: scripts/test-e2e.sh builds
        # them in some of its modes alone, and the guest tests use them
        # whenever they are set.
        CLADEFS_FIRECRACKER_SECCOMP_FILTER | CLADEFS_FIRECRACKER_JAILER) ;;
        # A daemon's control socket, statistics file and metrics address:
        # every extent daemon there reads them, those the tests start too, and
        # all of them would serve at the one socket and address, and write the
        # one file.
        CLADEFSD_GRPC | CLADEFSD_STATS | CLADEFSD_METRICS) ;;
        # The directories a daemon serves cells in and puts its tenants'
        # sockets in: every extent daemon there would take them, those the
        # tests start too, whose cells are in directories of their own, and
        # with none of these roots there, a daemon refuses every cell.
        CLADEFSD_ROOTS | CLADEFSD_TENANTS) ;;
        # Where the benchmark's cases write their measurements, wherever they
        # run; bench-linux-host.sh alone sets it.
        CLADEFS_BENCH_RESULTS) ;;
        # Which node the driver runs on, and the workspace's /work and repo: the driver's own.
        CLADEFS_TEST_HOST | CLADEFS_HOTBED_WORK | CLADEFS_HOTBED_REPO) ;;
        *) return 1 ;;
    esac
}

# host_job [NAME=VALUE...] SCRIPT [ARG...] runs the host script SCRIPT as the run's job
# (host_command), and waits for it (host_wait): its output is its log, and its exit status is this
# function's.
#
# A measurement's (host_bench=1) is two commands, one after the other, each SCRIPT's, which tells
# them apart by CLADEFS_TEST_HOST_PHASE (scripts/lib/linux-host-setup.sh's host_phase): `build`,
# ordinary work beside others', its builds alone, into the run's work directory, its log
# RESULTS/remote-build.log; then, if it passed, `measure`, alone under hotbed run --bench, against
# what the build made, building nothing, which scripts/lib/measurement.sh refuses there. A build
# inside the measurement held its cores and memory from every other measurement for as long as it
# compiled, an hour on 4 cores (TASK-330.4, TASK-368). A driver stopped while its run builds leaves
# its `resume` the build alone: run the driver again. Any other run's is one command, of no phase.
host_job() {
    if (( ! host_bench )); then
        host_launch CLADEFS_TEST_HOST_PHASE= "$@"
        return
    fi
    host_bench=0 host_launch CLADEFS_TEST_HOST_PHASE=build "$@" \
        > "$results/remote-build.log" 2>&1 || return
    host_launch CLADEFS_TEST_HOST_PHASE=measure "$@"
}

# host_launch [NAME=VALUE...] SCRIPT [ARG...] runs one command of the run, as a measurement if
# host_bench is 1, and waits for it, as host_job says.
host_launch() {
    local placement flags=() assignment said status=0
    host_command "$@" || return
    if [[ $host == local ]]; then
        host_state=launched
        env ${host_environment[@]+"${host_environment[@]}"} timeout --signal=TERM --kill-after=30s \
            "${CLADEFS_TEST_HOST_LIMIT:-$limit}" "${host_argv[@]}" || status=$?
        host_state=ended
        return "$status"
    fi
    host_placement
    # cladefs: a measurement holds its guests' and daemon's memory with little CPU, which hotbed
    # would stop as idle (`--keep-idle`); and reserves its peak, CLADEFS_TEST_HOST_PEAK, where work,
    # its builds, reserves CLADEFS_TEST_HOST_BUILD_PEAK (default 8G): hotbed also stops a command
    # holding much more than its CPU use warrants.
    # A run of work that holds guests, unisolated, keeps them as a measurement does, if asked
    # (CLADEFS_TEST_HOST_KEEP_IDLE=1).
    local peak=${CLADEFS_TEST_HOST_BUILD_PEAK:-8G}
    if (( host_bench )); then
        flags+=(--bench --keep-idle)
        peak=${CLADEFS_TEST_HOST_PEAK:-12G}
    elif [[ ${CLADEFS_TEST_HOST_KEEP_IDLE:-} == 1 ]]; then
        flags+=(--keep-idle)
    fi
    if [[ -n ${CLADEFS_TEST_HOST_MEM:-} ]]; then flags+=(--mem-high "$CLADEFS_TEST_HOST_MEM"); fi
    for assignment in ${host_environment[@]+"${host_environment[@]}"}; do
        flags+=(--env "$assignment")
    done
    # cladefs: hotbed reserves a detached command's peak memory on its node, which it needs said
    # (above); and its refusal, JSON on stdout, is said here, not swallowed with it.
    said=$(host_hotbed run --no-sync --detach --json ${placement[@]+"${placement[@]}"} --kind test \
        --mem "$peak" --hard-limit "${CLADEFS_TEST_HOST_LIMIT:-$limit}" \
        ${flags[@]+"${flags[@]}"} -- "${host_argv[@]}") || {
        status=$?
        echo "hotbed refused the run's job: $said" >&2
        return "$status"
    }
    host_command_id=$(python3 -c 'import json, sys; print(json.load(sys.stdin)["data"]["id"])' \
        <<< "$said") || return
    host_state=launched
    host_side note "$host_dir" "$host_command_id" > /dev/null ||
        echo "keeping $host_run's command, $host_command_id, failed: resume can't find it" >&2
    host_wait
}

# host_wait waits for the run's job, as host_job does, for one whose driver stopped waiting for it
# too: its output, then its exit status, hotbed's (124 if its time limit paused it).
host_wait() {
    local status=0
    host_hotbed logs -f "$host_command_id" || status=$?
    host_state=ended
    return "$status"
}

# host_fetch copies the run's results in the workspace into the results, as one archive
# (host-workspace.sh's fetch). A tar that fails there may still have written the archive of every
# other file (1: a file changed as it read it, as the logs of a daemon a killed host script left
# running do; 2: it could not read one): so the archive is unpacked however it ended, and the first
# failure is this function's.
host_fetch() {
    local statuses
    host_side fetch "$host_dir" 2>> "$results/remote.log" | base64 --decode | tar -xzf - -C "$results"
    statuses=("${PIPESTATUS[@]}")
    return $(( statuses[0] ? statuses[0] : statuses[1] ? statuses[1] : statuses[2] ))
}

# host_end removes the run's directory from the workspace, its worktrees with it, once the run has
# one, and ends the session the run started: but for a job that runs on, its driver having stopped
# waiting for it, which it says how to wait for again.
host_end() {
    case $host_state in
        '')
            if (( ! host_keep_session )); then host_session_end || return; fi
            return 0
            ;;
        launched)
            echo "$host_run runs on, hotbed command $host_command_id: wait for it and fetch its" \
                "results with \`$0 resume $host_run ${results:-RESULTS}\`, or end it with" \
                "\`$0 cancel $host_run\`." >&2
            return 0
            ;;
    esac
    if ! host_side clean "$host_dir"; then
        echo "Removing $host_dir from the workspace failed." >&2
        return 1
    fi
    host_state=''
    if (( ! host_keep_session )); then host_session_end || return; fi
}

# host_session_end ends the session the run started, if it did. A driver of several runs in one
# workspace, one after another, sets host_keep_session=1, and calls this itself once done.
host_keep_session=0
host_session_end() {
    if (( host_session_ours )); then
        host_hotbed session end > /dev/null || { echo "Ending session $host_session failed." >&2; return 1; }
        host_session_ours=0 host_session=${HOTBED_SESSION:-}
    fi
}

# host_exit, the trap host_start sets, ends the run (host_end) as the driver exits; a failure to
# fails the driver. A driver that sets a trap of its own calls host_end in it.
host_exit() {
    local status=$?
    trap - EXIT
    host_end || status=1
    exit "$status"
}

# host_actions [ACTION [ARG...]] does the actions a driver takes of a run, which a driver calls
# with its arguments before its own: `resume RUN RESULTS` waits for the job of the run RUN, whose
# driver stopped waiting for it, fetches its results to RESULTS, runs host_after's function, and
# ends the run, as the driver would have; `cancel RUN` ends the job and the run. Each runs in the
# session bound to this checkout, on CLADEFS_TEST_HOST's node or where that session's workspace
# is, and exits; given any other arguments, this returns.
host_actions() {
    local status=0
    case ${1:-} in
        resume)
            (( $# == 3 )) || { echo "Usage: $0 resume RUN RESULTS" >&2; exit 2; }
            [[ $2 =~ $host_run_pattern ]] || { echo "Name the run, run-..." >&2; exit 2; }
            host_name "$2"
            results=$3
            mkdir -p "$results"
            results=$(cd "$results" && pwd)
            echo "Results: $results" >&2
            host_session_ensure || exit
            host_command_id=$(host_side job "$host_dir") || exit
            host_state=launched
            trap host_exit EXIT
            host_wait >> "$results/remote.log" 2>&1 || status=$?
            host_fetch || status=$?
            if [[ -n $host_after ]]; then "$host_after" "$status" || status=$?; fi
            exit "$status"
            ;;
        cancel)
            (( $# == 2 )) || { echo "Usage: $0 cancel RUN" >&2; exit 2; }
            [[ $2 =~ $host_run_pattern ]] || { echo "Name the run, run-..." >&2; exit 2; }
            host_name "$2"
            host_session_ensure || exit
            host_command_id=$(host_side job "$host_dir") || exit
            host_hotbed stop "$host_command_id" > /dev/null || exit
            host_state=ended
            host_end || exit
            echo "Ended $host_run." >&2
            exit 0
            ;;
    esac
}
