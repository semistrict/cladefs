# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Runs cladefs's host scripts (scripts/lib/*-linux-host.sh) on a Linux host of one's own, over ssh,
# for the drivers that source this: scripts/test-host.sh, bench-host.sh, bench-ab-host.sh,
# gets-host.sh, mutants-host.sh, fork-time-host.sh, gc-sweep-host.sh and migrate-host.sh; and
# through test-host.sh, perf-gate-host.sh, soak-host.sh, soak-workloads-host.sh,
# guest-mutants-host.py and dax-gate.sh.
#
# The host, CLADEFS_HOST as ssh names it (default ubu1), or `local`, this machine itself, reached
# without ssh (as the nightly on ubu1 runs, from cron: scripts/nightly-host.sh), is an x86-64
# Linux host with KVM, set up
# once by its own setup, not by a run (ubu1's ~/docs/system-setup.d: 140-fxdax_dev.sh's packages,
# those scripts/lib/linux-host-setup.sh lists, and the user's Rust, in ~/.cargo, with
# cargo-nextest; 141's sudo without a password; 143's ext4 scratch disk, /mnt/fxdax-scratch; 146's
# fxjob; 147's cargo-mutants, perf and bpftrace), and whose clone of cladefs, CLADEFS_HOST_REPO (a path
# in its home, default src/fxdax), fetches from cladefs's origin. Others' runs share it.
#
# A run is a directory on the host, RUN under CLADEFS_HOST_SCRATCH (default
# /mnt/fxdax-scratch/fxdax-host), RUN the run's name (host_run): a worktree of the commit run, which
# origin has (host_commit), its vendor/firecracker checked out; the results; and the work
# directory; the host script builds in the worktree, its target directories its own
# (scripts/lib/host-remote.sh, which does what a run does there). Its job, the host script
# (host_command), runs apart from the SSH session, under fxjob, in a cgroup of its own and within
# CLADEFS_HOST_MEM (default 12G), and with fxjob --bench, alone and on CPUs of its own, if it is a
# measurement (host_bench=1), which builds first, in a job of work of its own (host_job); as root,
# as its tests need, with the user's Rust; its builds, and nextest's tests, at most
# CLADEFS_HOST_JOBS at once (default 4), at niceness CLADEFS_HOST_NICE (default 19); its guest kernels taken from the host's cache of those built from
# the same sources (host_kernel, scripts/lib/linux-host-setup.sh); and nothing of it left running
# once it ends, root's processes too (host-remote.sh's reap). The driver waits for the job, reconnecting,
# fetches the results, and removes the run's directory, its worktrees with it, whatever happens:
# but for a job whose driver stopped waiting for it, which runs on, for the driver's `resume` to
# wait for again and fetch, or its `cancel` to end (host_actions).
#
# The sourcing script sets repo, the tree whose scripts run, before it sources this, which names
# the run; then results, the local directory the results come back to, and limit, the longest the
# job may take (timeout's DURATION; CLADEFS_HOST_LIMIT, if set, in its place), and calls host_start,
# host_job and host_fetch. CLADEFS_HOST_RUN is the run's number, as its caller counts runs (such as
# r1040), which its name carries. CLADEFS_HOST_COMMIT names the commit run (default HEAD, of a tree
# that changes nothing it tracks), and CLADEFS_HOST_PATCH a patch the run applies to it (as
# scripts/guest-mutants-host.py's mutants). CLADEFS_GUEST_KERNEL names a guest kernel here, which the
# run copies to the host and boots.
#
# The scratch disk is every job's on the host, and others' runs may fill it. A run that names the
# room it needs there, in bytes, checks the disk has it before it builds, CLADEFS_HOST_ROOM (in
# host_start, before its directory is made), and, a measurement, before it measures,
# CLADEFS_HOST_ROOM_MEASURE (host_job): short of it, it stops there, ending 28, rather than fail
# midway, as the nightly's first night did, whose measurement could not write even its status
# (TASK-489). Each check is a line of RESULTS/room.txt, `PHASE FREE NEED ok|short DISK`, PHASE
# build or measure, DISK where the disk is mounted; and, once a measurement's build ended, the
# bytes its run's directory holds, `built BYTES`, which tells a later run what a build takes.
# CLADEFS_HOST_SWEEP=1 first removes the runs of earlier drivers of the same name,
# CLADEFS_HOST_RUN, whose job is not running (host-remote.sh's sweep), as a driver that runs under
# one name, one at a time, needs: the nightly (scripts/nightly-host.sh).
#
# The drivers run these functions under `||`, where errexit stops nothing, so each checks the
# status of its every step itself.

host=${CLADEFS_HOST:-ubu1}
[[ $host =~ ^[A-Za-z0-9._@-]+$ ]] || { echo "CLADEFS_HOST names a host, as ssh does: $host" >&2; exit 2; }
host_repo=${CLADEFS_HOST_REPO:-src/fxdax}
host_scratch=${CLADEFS_HOST_SCRATCH:-/mnt/fxdax-scratch/fxdax-host}
host_library=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# This run's mark, which its name and the names of its results end in.
host_mark=$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
[[ ${CLADEFS_HOST_RUN:-} =~ ^[a-z0-9]*$ ]] ||
    { echo "CLADEFS_HOST_RUN names a run in lower-case letters and digits, such as r1040" >&2; exit 2; }
# What a run's name looks like, for those given one (host_actions).
host_run_pattern='^run-([a-z0-9]+-)?[0-9]{8}T[0-9]{6}Z-[0-9a-f]{8}$'
# Whether the run is a measurement, which fxjob --bench runs alone: a timed driver's always, any
# other's with CLADEFS_HOST_BENCH=1 (as scripts/perf-gate-host.sh's runs of test-host.sh). A
# measurement's host_job is two jobs of the run, its builds as work, then the measurement alone,
# building nothing (host_job, TASK-368).
host_bench=${CLADEFS_HOST_BENCH:-0}
[[ $host_bench =~ ^[01]$ ]] || { echo "CLADEFS_HOST_BENCH is 0 or 1" >&2; exit 2; }
# Where the run stands: '' (nothing of it on the host), prepared (its directory made), launched
# (its job running, as far as this knows) or ended.
host_state=''
# A function the driver names, which host_actions' resume runs as the driver's run would once the
# results are back, with the run's status, and whose status is the resume's.
host_after=''
host_ssh_options=(-o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=60
    -o ServerAliveCountMax=10)

# host_name RUN names the run RUN: its directory on the host is host_dir.
host_name() {
    host_run=$1
    # shellcheck disable=SC2034 # The drivers read it.
    host_dir=$host_scratch/$host_run
}
# The run's name: CLADEFS_HOST_RUN, if set; when it started, to the second; and this driver's mark,
# so that two drivers started in the same second keep apart (TASK-79.86).
host_name "run-${CLADEFS_HOST_RUN:+$CLADEFS_HOST_RUN-}$(date -u +%Y%m%dT%H%M%SZ)-$host_mark"

# host_label prints the host's name for the names of results: ssh's, without its user; a local
# host's own, as its name for itself.
host_label() {
    if [[ $host == local ]]; then hostname -s; else echo "${host##*@}"; fi
}

# host_shell COMMAND runs the shell command COMMAND on the host, with this function's standard
# input: over ssh, with keepalives, as a command may run for hours without output; or, on a local
# host, in a login's place, the home, as ssh runs it.
host_shell() {
    if [[ $host == local ]]; then
        (cd && bash -c "$1")
    else
        # shellcheck disable=SC2029 # COMMAND is built here, its words quoted for the host.
        ssh "${host_ssh_options[@]}" "$host" "$1"
    fi
}

# host_ssh COMMAND... runs COMMAND on the host.
host_ssh() {
    host_shell "$*" < /dev/null
}

# host_side ACTION ARG... does ACTION of scripts/lib/host-remote.sh on the host.
host_side() {
    host_shell "bash -s -- $(printf '%q ' "$@")" < "$host_library/host-remote.sh"
}

# host_put FILE PATH copies FILE here to PATH on the host.
host_put() {
    host_shell "mkdir -p $(printf %q "$(dirname "$2")") && cat > $(printf %q "$2")" < "$1"
}

# host_commit [REV] prints the commit REV names in repo, in full; with no REV, CLADEFS_HOST_COMMIT's,
# or HEAD's, of a tree that changes nothing it tracks, as what it changes would not run.
# shellcheck disable=SC2154 # The sourcing script sets repo.
host_commit() {
    local rev=${1:-${CLADEFS_HOST_COMMIT:-}}
    if [[ -z $rev ]]; then
        if [[ -n $(git -C "$repo" status --porcelain --untracked-files=no) ]]; then
            echo "$repo changes what it tracks, which would not run: commit and push it, or name" \
                "a commit with CLADEFS_HOST_COMMIT." >&2
            return 2
        fi
        rev=HEAD
    fi
    git -C "$repo" rev-parse --verify --quiet "$rev^{commit}" ||
        { echo "$rev: no commit of $repo's" >&2; return 2; }
}

# host_start [LABEL=REV...] makes the run's directory on the host (host-remote.sh's prepare): the
# worktree of its commit (host_commit), CLADEFS_HOST_PATCH applied to it, and one of each REV, which
# the host script finds in host_dir/trees/LABEL; and records each in the results, source.txt, and
# LABEL/source.txt, with the run, the host and the directory there (run.txt). From here, the run's
# directory goes as the driver ends, whatever happens (host_exit).
host_start() {
    local main pairs=() pair sha label firecracker prepared out kernel patch
    main=$(host_commit) || return
    for pair in "$@"; do
        [[ $pair =~ ^[a-z0-9][a-z0-9-]*=.+$ ]] ||
            { echo "$pair: LABEL=COMMIT, LABEL in lower-case letters, digits and -" >&2; return 2; }
        sha=$(host_commit "${pair#*=}") || return
        pairs+=("${pair%%=*}=$sha")
    done
    mkdir -p "$results" || return
    results=$(cd "$results" && pwd) || return
    if [[ ${CLADEFS_HOST_SWEEP:-} == 1 ]]; then host_sweep || return; fi
    host_room build CLADEFS_HOST_ROOM || return
    host_state=prepared
    trap host_exit EXIT
    if [[ -n ${CLADEFS_HOST_PATCH:-} ]]; then
        host_put "$CLADEFS_HOST_PATCH" "$host_dir/patch" || return
        patch=$(shasum -a 256 < "$CLADEFS_HOST_PATCH") || return
    fi
    if [[ -n ${CLADEFS_GUEST_KERNEL:-} ]]; then
        host_put "$CLADEFS_GUEST_KERNEL" "$host_dir/guest-kernel" || return
        kernel=$(shasum -a 256 < "$CLADEFS_GUEST_KERNEL") || return
    fi
    prepared=$(host_side prepare "$host_dir" "$host_repo" "$main" ${pairs[@]+"${pairs[@]}"}) ||
        return
    printf 'run %s\nhost %s\ndirectory %s\n' "$host_run" "$host" "$host_dir" > "$results/run.txt" ||
        return
    while read -r label sha firecracker; do
        out=$results
        if [[ $label != . ]]; then out=$results/$label; fi
        mkdir -p "$out" || return
        {
            echo "revision $sha"
            echo "firecracker $firecracker"
            if [[ $label == . && -n ${patch:-} ]]; then
                echo "patch ${patch%% *} $(basename "$CLADEFS_HOST_PATCH")"
            fi
            if [[ $label == . && -n ${kernel:-} ]]; then echo "guest kernel ${kernel%% *}"; fi
        } > "$out/source.txt" || return
    done <<< "$prepared"
}

# host_sweep removes the runs of earlier drivers of CLADEFS_HOST_RUN's name whose job is not
# running (host-remote.sh's sweep).
host_sweep() {
    [[ -n ${CLADEFS_HOST_RUN:-} ]] ||
        { echo "CLADEFS_HOST_SWEEP sweeps the runs of CLADEFS_HOST_RUN's name: name them" >&2
          return 2; }
    host_side sweep "$host_scratch" "$host_repo" "$CLADEFS_HOST_RUN"
}

# host_room PHASE VARIABLE checks the scratch disk has the bytes the variable VARIABLE names free,
# if it names any, before the run's PHASE, build or measure, and records it in RESULTS/room.txt;
# short of it, it says so and fails, 28.
host_room() {
    local phase=$1 variable=$2 need=${!2:-} said free disk verdict=ok
    [[ -n $need ]] || return 0
    [[ $need =~ ^[0-9]+$ ]] || { echo "$variable is a count of bytes: $need" >&2; return 2; }
    said=$(host_side room "$host_scratch") || return
    read -r free disk <<< "$said"
    if (( free < need )); then verdict=short; fi
    echo "$phase $free $need $verdict $disk" >> "$results/room.txt" || return
    if [[ $verdict == short ]]; then
        echo "Not enough room: the scratch disk $disk has $free bytes free before the run" \
            "${phase}s, which needs $need ($variable)." >&2
        return 28
    fi
}

# host_built records in RESULTS/room.txt the bytes the run's directory holds once its build ended,
# for a run that checks its room (host_room). A failure to tell is said, and measures on.
host_built() {
    local size
    [[ -n ${CLADEFS_HOST_ROOM:-} ]] || return 0
    if size=$(host_side size "$host_dir"); then
        echo "built $size" >> "$results/room.txt"
    else
        echo "The size of $host_dir, built, is unknown (above): room.txt lacks it." >&2
    fi
}

# host_command [NAME=VALUE...] SCRIPT [ARG...] prints the job's command, which runs the host script
# scripts/lib/SCRIPT of the run's worktree, as root, with the user's Rust, for at most $limit (or
# CLADEFS_HOST_LIMIT), with the worktree, the results and the ARGs.
#
# The script runs in this machine's environment: every CLADEFS_*, CLADEFSD_* and PJDFSTEST_* variable set here,
# but those that stay here (host_env_local); the run's own, its work directory, the host's cache of
# guest kernels and, with CLADEFS_GUEST_KERNEL set, the copy host_start made; and each NAME=VALUE in
# place of the variable NAME. A value starting with ~/ names a path in the host's home. An empty
# value sends nothing, leaving NAME unset there, which its readers take for their default (an
# empty CLADEFS_ZSTD_LEVEL is an error).
host_command() {
    local jobs=${CLADEFS_HOST_JOBS:-4} nice=${CLADEFS_HOST_NICE:-19} given=() assignments=() name value
    local i j
    [[ $jobs =~ ^[1-9][0-9]*$ ]] || { echo "CLADEFS_HOST_JOBS is a count" >&2; return 2; }
    [[ $nice =~ ^[0-9]+$ ]] || { echo "CLADEFS_HOST_NICE is a niceness" >&2; return 2; }
    # A measurement at the host's own niceness: it builds nothing, and runs alone.
    if (( host_bench )); then nice=0; fi
    given=("CLADEFS_GUEST_KERNEL_JOBS=$jobs")
    for name in ${!CLADEFS_@} ${!CLADEFSD_@} ${!PJDFSTEST_@}; do
        if ! host_env_local "$name"; then
            given+=("$name=${!name}")
        fi
    done
    given+=(CLADEFS_HOST_PROVISIONED=1 CLADEFS_HOST_IN_PLACE=1 "CLADEFS_HOST_WORK=$host_dir/work"
        "CLADEFS_HOST_KERNELS=$host_scratch/kernels")
    if [[ -n ${CLADEFS_GUEST_KERNEL:-} ]]; then
        given+=("CLADEFS_GUEST_KERNEL=$host_dir/guest-kernel")
    fi
    while (( $# > 0 )) && [[ $1 =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; do
        given+=("$1")
        shift
    done
    local script=${1:?a host script required}
    shift
    for (( i = 0; i < ${#given[@]}; i++ )); do
        name=${given[i]%%=*} value=${given[i]#*=}
        # A later value of the name replaces this one.
        for (( j = i + 1; j < ${#given[@]}; j++ )); do
            if [[ ${given[j]%%=*} == "$name" ]]; then continue 2; fi
        done
        # shellcheck disable=SC2088,SC2016 # ~/ as it is, and $HOME for the host's shell to expand.
        if [[ -z $value ]]; then
            continue
        elif [[ $value == "~/"* ]]; then
            assignments+=("$(printf '%s="$HOME"/%q' "$name" "${value#"~/"}")")
        else
            assignments+=("$(printf '%s=%q' "$name" "$value")")
        fi
    done
    if (( nice )); then printf 'nice -n %d ' "$nice"; fi
    # shellcheck disable=SC2016 # For the host's shell to expand.
    # Root's git, in the user's worktrees, which it would refuse as another's (safe.directory).
    # The job's TMPDIR, fxjob's, which it removes with the job: sudo, set-user-ID, never passes it
    # on (glibc drops it, whatever sudo keeps).
    printf '%s' 'sudo env PATH="$HOME/.cargo/bin:$PATH" HOME="$HOME" TMPDIR="${TMPDIR:-/tmp}"' \
        " CARGO_BUILD_JOBS=$jobs" \
        " NEXTEST_TEST_THREADS=$jobs GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory" \
        " GIT_CONFIG_VALUE_0='*'" \
        ${assignments[@]+"${assignments[@]/#/ }"}
    printf ' %q' timeout --signal=TERM --kill-after=30s "${CLADEFS_HOST_LIMIT:-$limit}" bash \
        "$host_dir/tree/scripts/lib/$script" "$host_dir/tree" "$host_dir/results" "$@"
    echo
}

# host_env_local NAME succeeds for the variables that stay here, though host_command would send
# them by their names: those this machine sets up its own builds, daemons and runs with, and which
# the host scripts, or what they run, would take as they are. Set here, they name what this machine
# built or fetched, its own daemon's sockets, address and directories, and where its own runs
# write; sent, every build, test and daemon on the host would use them in place of its own. Those
# the host scripts always set themselves, such as CLADEFS_FIRECRACKER and CLADEFSD
# (scripts/test-e2e.sh), need no place here. The others go: knobs a driver's caller sets for the
# host, what runs and how, and where its stores keep their branches and its daemons what they read
# from them (CLADEFSD_REMOTE, CLADEFS_S3_URL, CLADEFSD_CACHE), or a driver's own, which
# nothing there reads.
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
        # The directories a daemon serves stores in and puts its tenants'
        # sockets in: every extent daemon there would take them, those the
        # tests start too, whose stores are in directories of their own, and
        # with none of these roots there, a daemon refuses every store.
        CLADEFSD_ROOTS | CLADEFSD_TENANTS) ;;
        # Where the benchmark's cases write their measurements, wherever they
        # run; bench-linux-host.sh alone sets it.
        CLADEFS_BENCH_RESULTS) ;;
        *) return 1 ;;
    esac
}

# host_job [NAME=VALUE...] SCRIPT [ARG...] runs the host script SCRIPT as the run's job
# (host_command), and waits for it (host_wait): its log, once it ends, is its output, and its exit
# status is this function's.
#
# A measurement's (host_bench=1) is two jobs, one after the other, each SCRIPT's, which tells them
# apart by CLADEFS_HOST_PHASE (scripts/lib/linux-host-setup.sh's host_phase): `build`, ordinary
# work beside others' (fxjob without --bench, at CLADEFS_HOST_NICE, in 12G), its builds alone,
# into the run's work directory, its log RESULTS/remote-build.log; then, if it passed, `measure`,
# alone under fxjob --bench, against what the build made, building nothing, which
# scripts/lib/measurement.sh refuses there. A build inside the measurement held its cores and
# memory from every other measurement for as long as it compiled, an hour on 4 cores (TASK-330.4,
# TASK-368). A driver stopped while its run builds leaves its `resume` the build alone: run the
# driver again. Any other run's is one job, of no phase. Between the two, the run checks its room
# for the measurement (host_room).
host_job() {
    if (( ! host_bench )); then
        host_launch "${CLADEFS_HOST_MEM:-12G}" CLADEFS_HOST_PHASE= "$@"
        return
    fi
    # The builds in a work job's memory, whatever the measurement takes (CLADEFS_HOST_MEM, 16G
    # for the qualification's).
    host_bench=0 host_launch 12G CLADEFS_HOST_PHASE=build "$@" \
        > "$results/remote-build.log" 2>&1 || return
    host_built
    host_room measure CLADEFS_HOST_ROOM_MEASURE || return
    host_launch "${CLADEFS_HOST_MEM:-12G}" CLADEFS_HOST_PHASE=measure "$@"
}

# host_launch MEMORY [NAME=VALUE...] SCRIPT [ARG...] runs one job of the run, in MEMORY, as a
# measurement if host_bench is 1, and waits for it, as host_job says. A job whose launching SSH
# failed is waited for only if a fresh connection finds it launched, its pid kept: that SSH may
# drop once the job runs, as GCE r942's did (TASK-235); one that didn't start returns that SSH's
# status.
host_launch() {
    local memory=$1 command launched=0
    shift
    command=$(host_command "$@") || return
    host_put <(printf '%s\n' "$command") "$host_dir/job/command" || return
    host_state=launched
    host_side launch "$host_dir" "$host_run" "$host_bench" "$memory" || launched=$?
    if (( launched )) && ! host_ssh "test -e $(printf %q "$host_dir/job/pid")"; then
        host_state=prepared
        return "$launched"
    fi
    host_wait
}

# host_wait waits for the run's job, as host_job does, for one whose driver stopped waiting for it
# too: reconnecting as a connection drops, and giving up after 20 tries none of which connected,
# the job running on. A job whose shell is gone without its status, as one is when its host
# restarted under it, is lost; so is one whose status file holds no number, as a full disk leaves
# it (host-remote.sh's wait, TASK-489).
host_wait() {
    local said why tries=0
    while true; do
        said=$(host_side wait "$host_dir" 2> /dev/null) || true
        case ${said##*$'\n'} in
            "status "*)
                host_state=ended
                host_side log "$host_dir" || echo "fetching $host_run's log failed" >&2
                return "${said##*status }"
                ;;
            lost | "lost unreadable")
                host_state=ended
                why=''
                if [[ $said == *unreadable ]]; then
                    why=" (its status file empty or unreadable: was its disk full?)"
                fi
                echo "$host_run on $host is lost: its job ended without its status$why." >&2
                host_side log "$host_dir" || echo "fetching $host_run's log failed" >&2
                return 1
                ;;
            none)
                host_state=prepared
                echo "$host_run on $host has no job." >&2
                return 1
                ;;
        esac
        # Not connected, or the connection dropped: again, but not without end.
        if [[ $said == waiting* ]]; then tries=0; fi
        tries=$((tries + 1))
        if (( tries >= 20 )); then
            echo "$host does not answer: $host_run's job may run on." >&2
            return 1
        fi
        sleep 30
    done
}

# host_fetch copies the run's results on the host into the results, as one archive (host-remote.sh's
# fetch). A tar that fails there may still have written the archive of every other file (1: a file
# changed as it read it, as the logs of a daemon a killed host script left running do; 2: it could
# not read one): so the archive is unpacked however it ended, and the first failure is this
# function's.
host_fetch() {
    local statuses
    host_side fetch "$host_dir" 2>> "$results/remote.log" | tar -xzf - -C "$results"
    statuses=("${PIPESTATUS[@]}")
    return $(( statuses[0] ? statuses[0] : statuses[1] ))
}

# host_end removes the run's directory from the host, its worktrees with it, once the run has one:
# but for a job that runs on, its driver having stopped waiting for it, which it says how to wait
# for again.
host_end() {
    case $host_state in
        '') return 0 ;;
        launched)
            echo "$host_run runs on, on $host: wait for it and fetch its results with" \
                "\`$0 resume $host_run ${results:-RESULTS}\`, or end it with \`$0 cancel $host_run\`." >&2
            return 0
            ;;
    esac
    if ! host_side clean "$host_dir" "$host_repo"; then
        echo "Removing $host_dir from $host failed." >&2
        return 1
    fi
    host_state=''
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
# ends the run, as the driver would have; `cancel RUN` ends the job and the run. Each exits; given
# any other arguments, this returns.
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
            host_state=launched
            trap host_exit EXIT
            host_wait >> "$results/remote.log" 2>&1 || status=$?
            if [[ $host_state == launched ]]; then exit "$status"; fi
            host_fetch || status=$?
            if [[ -n $host_after ]]; then "$host_after" "$status" || status=$?; fi
            exit "$status"
            ;;
        cancel)
            (( $# == 2 )) || { echo "Usage: $0 cancel RUN" >&2; exit 2; }
            [[ $2 =~ $host_run_pattern ]] || { echo "Name the run, run-..." >&2; exit 2; }
            host_name "$2"
            host_side cancel "$host_dir" || exit
            host_state=ended
            host_end || exit
            echo "Ended $host_run on $host." >&2
            exit 0
            ;;
    esac
}
