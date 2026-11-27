# shellcheck shell=bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Sourced by the mutant scripts (crates/*/mutants.sh, scripts/loom-mutants.sh), run from the
# repository's root: whether a mutant's patch applies as it is, the same on every host, and putting
# it into the tree while its killer runs (patch_run, below). A patch applies with no fuzz, as GNU
# patch takes it (`-F0`), and BSD patch (macOS) is laxer: it takes a hunk whose header counts other
# lines than it has, and one with more context on one side of its changes than the other away from
# the file's ends, which GNU patch then anchors to the file's start or end. A patch re-anchored by
# hand on macOS checked there and failed on the test host (TASK-395).

# patch_well_formed PATCH: whether each of its hunks has the lines its header counts, and as many
# lines of context after its changes as before, but at the start or end of the file it changes.
patch_well_formed() {
    awk '
        function fail(msg) {
            printf "%s: %s\n", FILENAME, msg > "/dev/stderr"
            bad = 1
        }
        function close_hunk() {
            if (!hunk) return
            hunk = 0
            if (old > 0 || new > 0) {
                fail("the hunk at line " at " has fewer lines than its header counts")
            } else if (lead != trail && !(lead < trail && first == 1) &&
                       !(trail < lead && first + count - 1 == lines)) {
                fail("the hunk at line " at " has " lead " lines of context before its changes and " \
                     trail " after, away from the start and end of " file)
            }
        }
        !hunk && /^\+\+\+ / {
            file = $2
            sub(/^b\//, "", file)
            lines = 0
            while ((getline line < file) > 0) lines++
            close(file)
            after = 0
            next
        }
        /^@@ / {
            close_hunk()
            split($2, o, ",")
            split($3, n, ",")
            first = substr(o[1], 2) + 0
            count = (2 in o) ? o[2] + 0 : 1
            old = count
            new = (2 in n) ? n[2] + 0 : 1
            hunk = 1
            after = 1
            at = FNR
            lead = trail = changed = 0
            next
        }
        hunk && /^[ +-]/ {
            c = substr($0, 1, 1)
            if (c == " ") {
                old--
                new--
                if (changed) trail++; else lead++
            } else {
                changed = 1
                trail = 0
                if (c == "-") old--; else new--
            }
            if (old < 0 || new < 0) {
                fail("the hunk at line " at " has more lines than its header counts")
                hunk = 0
            } else if (old == 0 && new == 0) {
                close_hunk()
            }
            next
        }
        hunk && /^\\/ { next }
        hunk { close_hunk() }
        after && /^[ +-]/ && !/^(---|\+\+\+) / {
            fail("line " FNR " is past its hunk, as its header counts it")
        }
        END {
            close_hunk()
            exit bad
        }
    ' "$1"
}

# patch_round_trip PATCH: whether it is well formed, and applies and reverses, leaving the files it
# changes as they were, in a copy of them.
patch_round_trip() {
    local copy file status=0
    patch_well_formed "$1" || return 1
    copy=$(mktemp -d)
    for file in $(sed -n 's|^+++ b/||p' "$1"); do
        mkdir -p "$copy/$(dirname "$file")"
        cp "$file" "$copy/$file"
    done
    if ! patch -s -F0 -p1 -d "$copy" < "$1" > /dev/null; then
        echo "$1 doesn't apply" >&2
        status=1
    elif ! patch -s -F0 -R -p1 -d "$copy" < "$1" > /dev/null; then
        echo "$1 doesn't reverse" >&2
        status=1
    else
        for file in $(sed -n 's|^+++ b/||p' "$1"); do
            cmp -s "$file" "$copy/$file" || { echo "$1: $file isn't as it was, reversed" >&2; status=1; }
        done
    fi
    rm -r "$copy"
    return $status
}

# Mutants in the tree. A runner puts a mutant into its working tree, runs the mutant's killer, and
# takes it out. Stopped in between (a run cancelled mid-build, ^C), it left the mutant in, and all
# built there from then on had the bug, to be committed with whatever else changed (TASK-444.2.5).
# patch_run takes the mutant out whatever stops the runner that it can see. For what it can't see
# (SIGKILL, a host gone), $MUTANT_MARK, at the tree's root, names the mutant while it is in, and the
# next patch_run takes it out first. A runner puts a patch into its tree only through patch_run
# (scripts/tests/test_patches.py checks).

MUTANT_MARK=.mutant-in

# patch_run PATCH LOG COMMAND...: COMMAND's status, run with PATCH in the tree and its output in
# LOG, the patch put in and taken out again. A SIGTERM, SIGINT or SIGHUP meanwhile stops COMMAND and all
# it started, takes the patch out, and exits the runner (143, 130, 129). Exits the runner (1) if the
# patch doesn't apply or reverse, or a mutant left in the tree can't be taken out.
patch_run() {
    local patch=$1 log=$2 status
    shift 2
    patch_recover || exit 1
    _patch_in="" _patch_child=""
    trap '_patch_stop 143' TERM
    trap '_patch_stop 130' INT
    trap '_patch_stop 129' HUP
    echo "$$ $patch" > "$MUTANT_MARK"
    if ! patch -s -F0 -p1 < "$patch"; then
        rm "$MUTANT_MARK"
        echo "$patch doesn't apply" >&2
        exit 1
    fi
    _patch_in=$patch
    # In a process group of its own, so that a stop reaches what COMMAND started (cargo's tests, a
    # script's builds), and the terminal's ^C reaches the runner, which stops COMMAND. Never the
    # terminal's own group, so not reading it: a read would stop it. The child makes its group
    # itself, before it runs COMMAND, not bash's job control (set -m), whose setpgid of the child
    # raced the child's own on macOS under load, bash printing "child setpgid (PID to PID):
    # Operation not permitted" (board 2954).
    # A command not found ends 127, as bash's would.
    python3 -c '
import os, sys
os.setpgid(0, 0)
try:
    os.execvp(sys.argv[1], sys.argv[1:])
except OSError as error:
    print(f"{sys.argv[1]}: {error.strerror}", file=sys.stderr)
    sys.exit(127)' "$@" < /dev/null > "$log" 2>&1 &
    _patch_child=$!
    wait "$_patch_child"
    status=$?
    _patch_child=""
    _patch_out
    trap - TERM INT HUP
    return $status
}

# patch_recover: takes out the mutant that a runner stopped by SIGKILL left in the tree, which
# $MUTANT_MARK names. Fails while a live runner holds the tree, or if the mutant doesn't reverse.
patch_recover() {
    local pid patch
    [[ -e $MUTANT_MARK ]] || return 0
    read -r pid patch < "$MUTANT_MARK"
    if kill -0 "$pid" 2> /dev/null; then
        echo "runner $pid has $patch in this tree; if it is gone, $MUTANT_MARK is stale: remove it" >&2
        return 1
    fi
    if ! patch -s -F0 -R -p1 < "$patch"; then
        echo "$patch, left in the tree by runner $pid, doesn't reverse:" \
            "take it out by hand, then remove $MUTANT_MARK" >&2
        return 1
    fi
    _patch_touch "$patch"
    rm "$MUTANT_MARK"
    echo "took out $patch, which runner $pid left in the tree when it was stopped" >&2
}

# _patch_stop STATUS: a signal's trap in patch_run. Stops COMMAND's process group, takes the patch
# out, exits with STATUS.
_patch_stop() {
    trap '' TERM INT HUP
    if [[ -n $_patch_child ]]; then
        # Its group, or, stopped before it made the group, the child alone.
        kill -TERM -- "-$_patch_child" 2> /dev/null || kill -TERM "$_patch_child" 2> /dev/null
        wait "$_patch_child"
    fi
    [[ -z $_patch_in ]] || _patch_out
    exit "$1"
}

# _patch_out: takes out the patch patch_run put in, or exits the runner, which then leaves the tree
# with it and $MUTANT_MARK naming it.
_patch_out() {
    if ! patch -s -F0 -R -p1 < "$_patch_in"; then
        echo "$_patch_in doesn't reverse: the tree has it" >&2
        exit 1
    fi
    # Its files rewritten, as patch writes them: cargo builds them anew, never the mutant again.
    _patch_touch "$_patch_in"
    _patch_in=""
    rm "$MUTANT_MARK"
}

_patch_touch() {
    sed -n 's|^+++ b/||p' "$1" | xargs touch
}

# mutants_names DIR [NAME...]: populates the caller's `names` array with NAME..., or all *.patch
# basenames under DIR when no names were given.
mutants_names() {
    local dir=$1 patch
    shift
    names=("$@")
    if [[ ${#names[@]} == 0 ]]; then
        for patch in "$dir"/*.patch; do
            names+=("$(basename "$patch" .patch)")
        done
    fi
}

# mutants_check DIR [--require-killer] NAME...: validates every patch in DIR (round-trip with zero
# fuzz, and optional `# Killer:` header), printing the summary and exiting non-zero if any broke.
mutants_check() {
    local dir=$1 require_killer=0 broken=0 name patch
    shift
    if [[ ${1:-} == --require-killer ]]; then
        require_killer=1
        shift
    fi
    for name in "$@"; do
        patch=$dir/$name.patch
        patch_round_trip "$patch" || broken=$((broken + 1))
        if [[ $require_killer == 1 ]]; then
            [[ -n $(sed -n 's/^# Killer: //p' "$patch") ]] || { echo "$patch names no killer" >&2; broken=$((broken + 1)); }
        fi
    done
    echo "mutants: $# checked, $broken broken"
    exit $((broken > 0))
}
