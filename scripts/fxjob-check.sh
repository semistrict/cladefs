#!/usr/bin/env bash
# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Checks fxjob's admission (scripts/fxjob) on a host set up for it (ubu1 or asahimbp, its user's
# systemd manager with fxdax.slice), beside whatever jobs the host runs, which it neither counts
# nor touches: its jobs run in slices of its own under fxdax.slice, on a budget of their own (300M,
# 10 CPUs, a measurement's 8), with a queue, history, measurements' lock and jobs' temporary directories of their own
# in a temporary directory, and their measurements isolate nothing (FXJOB_ISOLATION=0). Each job
# is a sleep of a few seconds, memory reserved, none used, but those that check what a job's
# temporary directory leaves: they mount file systems in it, which needs `sudo -n`, as reap does.
# It takes about a minute.
#
#   scripts/fxjob-check.sh            checks scripts/fxjob
#   FXJOB=/usr/local/bin/fxjob scripts/fxjob-check.sh
#                                     checks the one installed
#   FXJOB_CHECK_OOM=1 scripts/fxjob-check.sh
#                                     checks a job past its limit too: an OOM kill of 64M, which
#                                     the desktop reports as the system out of memory
#
# What it checks: a request past the budget fails at once, and a measurement on a host that takes
# none; jobs past the budget wait and start
# first come, first served, a small one never passing a large one ahead of it, as many at once
# as the budget holds, in memory and in CPUs; a job killed outright (kill -9) frees its
# reservation, and a waiting fxjob killed outright its place; a signal ends a waiting fxjob; a
# measurement waits for running work to leave it room, work after it waits behind it, and
# another measurement for it to end; a job started by another's command skips the queue; fxjob
# ps shows the waiting and the reservations without errors; a job's TMPDIR is a directory of its
# own, which, once a SIGKILL ended the job, is gone, with what was mounted in it, the loop devices
# on its files, and the memory its tmpfs took, which work's slice no longer holds; one whose fxjob
# was killed outright the next job's fxjob removes; a job's command reaches it as given, its $VAR
# and $$ unexpanded by systemd-run; every job that ran leaves a line of history,
# its status the command's, 137 for one its memory limit killed.
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fxjob=${FXJOB:-$repo/scripts/fxjob}
tmp=$(mktemp -d "${TMPDIR:-/tmp}/fxjob-check.XXXXXX")
export FXJOB_CONF=$tmp/fxjob.conf FXJOB_RUNTIME=$tmp/run FXJOB_HISTORY=$tmp/jobs.jsonl
export FXJOB_WORK_SLICE=fxdax-fxjobcheck.slice FXJOB_BENCH_SLICE=fxdax-fxjobcheckbench.slice
export FXJOB_BENCH_LOCK=$tmp/bench.lock FXJOB_ISOLATION=0 FXJOB_TMP=$tmp/jobs
printf 'MEM_BUDGET=300M\nCPU_BUDGET=10 # logical CPUs\nBENCH_CPUS=0-3,5,7-9\n' > "$FXJOB_CONF"
log=$tmp/log
: > "$log"

# What is left of the checks' jobs goes, theirs alone: their slices' scopes.
finish() {
  local pids
  pids=$(jobs -p)
  if [[ -n $pids ]]; then kill -KILL $pids 2> /dev/null || true; fi
  systemctl --user stop "$FXJOB_WORK_SLICE" "$FXJOB_BENCH_SLICE" 2> /dev/null || true
  # What a failed check left of a job's temporary directory: mounts, loop devices, root's files.
  findmnt -rn -o TARGET | grep -F "$tmp/" | sort -r | xargs -r sudo -n umount -l -- || true
  for backing in /sys/block/loop*/loop/backing_file; do
    if grep -qF "$tmp/" "$backing" 2> /dev/null; then
      backing=${backing#/sys/block/}
      sudo -n losetup -d "/dev/${backing%%/*}" || true
    fi
  done
  rm -rf "$tmp" 2> /dev/null || sudo -n rm -rf "$tmp"
}
trap finish EXIT

failures=0
check() { # DESCRIPTION COMMAND...: says whether COMMAND succeeds
  if "${@:2}"; then
    echo "ok    $1"
  else
    echo "FAIL  $1"
    failures=$((failures + 1))
  fi
}

# job NAME SECONDS FXJOB_OPTION...: runs a job in the background that sleeps SECONDS, logging its
# start and end; its fxjob's pid is pid[NAME], its standard error $tmp/NAME.err.
declare -A pid
job() {
  local name=$1 seconds=$2
  shift 2
  "$fxjob" --name "$name" "$@" -- bash -c '
    echo "start $1 $EPOCHREALTIME" >> "$2"
    sleep "$3"
    echo "end $1 $EPOCHREALTIME" >> "$2"' job "$name" "$log" "$seconds" 2> "$tmp/$name.err" &
  pid[$name]=$!
  # Each job's turn is the order they came in: let it take its place before the next.
  sleep 0.3
}
# at EVENT NAME: when the job NAME started or ended.
at() { awk -v e="$1" -v n="$2" '$1 == e && $2 == n { print $3 }' "$log"; }
# before EVENT1 NAME1 EVENT2 NAME2: whether NAME1's EVENT1 came before NAME2's EVENT2.
before() {
  local a b
  a=$(at "$1" "$2") b=$(at "$3" "$4")
  [[ -n $a && -n $b ]] && awk -v a="$a" -v b="$b" 'BEGIN { exit !(a < b) }'
}
# order NAME...: whether the jobs NAME started in that order.
order() { [[ $(awk '$1 == "start" { print $2 }' "$log" | grep -xF -f <(printf '%s\n' "$@") |
  paste -sd ' ') == "$*" ]]; }
# most NAME...: how many of the jobs NAME ran at once, at most.
most() {
  awk '{ print $3, $1, $2 }' "$log" | sort -n | grep -wF -f <(printf '%s\n' "$@") |
    awk '$2 == "start" { n++; if (n > m) m = n } $2 == "end" { n-- } END { print m + 0 }'
}
# finished NAME...: waits for the jobs NAME; status[NAME] their statuses.
declare -A status
finished() {
  local name
  for name in "$@"; do
    status[$name]=0
    wait "${pid[$name]}" || status[$name]=$?
  done
}
entries() { find "$FXJOB_RUNTIME/queue" -type f 2> /dev/null | wc -l; }

echo "== a request past the budget"
started=$EPOCHSECONDS
rc=0; "$fxjob" --mem 400M -- true 2> "$tmp/err" || rc=$?
check "--mem 400M of 300M fails, status 2" test $rc -eq 2
check "saying it would never start" grep -q 'never start' "$tmp/err"
rc=0; "$fxjob" --cpus 11 -- true 2> "$tmp/err" || rc=$?
check "--cpus 11 of 10 fails, status 2" test $rc -eq 2
printf 'CPU_BUDGET=4\nBENCH_CPUS=0-3,5,7-9\n' > "$tmp/small.conf"
rc=0; FXJOB_CONF=$tmp/small.conf "$fxjob" --bench --mem 1M -- true 2> "$tmp/err" || rc=$?
check "a measurement's 8 CPUs of 4 fails, status 2" test $rc -eq 2 -a "$(< "$tmp/err")" == \
  "fxjob: 8 CPUs, a measurement's, are more than all jobs together may reserve, CPU_BUDGET=4 ($tmp/small.conf): it would never start"
echo CPU_BUDGET=4 > "$tmp/nobench.conf"
rc=0; FXJOB_CONF=$tmp/nobench.conf "$fxjob" --bench --mem 1M -- true 2> "$tmp/err" || rc=$?
check "a measurement where the conf names no BENCH_CPUS fails, status 2" test $rc -eq 2 -a \
  "$(< "$tmp/err")" == "fxjob: this host takes no measurements: $tmp/nobench.conf names no BENCH_CPUS"
check "at once, no place taken in the queue" test $((EPOCHSECONDS - started)) -le 2 -a "$(entries)" -eq 0

echo "== six jobs of 100M on 300M"
names=(n1 n2 n3 n4 n5 n6)
for name in "${names[@]}"; do job "$name" 2 --mem 100M --cpus 1; done
finished "${names[@]}"
check "start in the order they came" order "${names[@]}"
check "three at once at most" test "$(most "${names[@]}")" -eq 3
check "n4 starts once one of the first three ended" before end n1 start n4
check "n4 said it waited" grep -q 'waits' "$tmp/n4.err"

echo "== a small job behind a large one waits its turn"
job big1 3 --mem 200M --cpus 1
job big2 1 --mem 200M --cpus 1
job small 1 --mem 50M --cpus 1
"$fxjob" ps > "$tmp/ps" 2> "$tmp/ps.err" && rc=0 || rc=$?
check "fxjob ps succeeds, printing no errors" test $rc -eq 0 -a ! -s "$tmp/ps.err"
check "fxjob ps lists big2 first, then small, waiting" \
  grep -qzE 'big2-[0-9]+ +200M +1 .*small-[0-9]+ +50M +1 ' "$tmp/ps"
check "fxjob ps says what is reserved" grep -q 'reserved 200M of 300M, 1 of 10 CPUs, 1 jobs' "$tmp/ps"
finished big1 big2 small
check "small, though it fitted beside big1, starts after big2" order big1 big2 small
check "small starts once big1 ended" before end big1 start small

echo "== CPUs"
job c1 2 --mem 10M --cpus 6
job c2 1 --mem 10M --cpus 6
finished c1 c2
check "6 CPUs wait for 6 of 10 to end" before end c1 start c2

echo "== a job killed outright frees its reservation"
setsid "$fxjob" --name k1 --mem 250M --cpus 1 -- sleep 300 2> "$tmp/k1.err" &
pid[k1]=$!
sleep 1
job k2 1 --mem 100M --cpus 1
sleep 1
check "k2 waits while k1 runs" test -z "$(at start k2)"
killed=$EPOCHREALTIME
kill -KILL -- "-${pid[k1]}"
finished k1 k2
check "k1's job ended by its SIGKILL" test "${status[k1]}" -eq 137
check "k2 starts within 3 s of it" awk -v a="$killed" -v b="$(at start k2)" 'BEGIN { exit !(b - a < 3) }'
check "k1's scope is gone" bash -c "! systemctl --user list-units --plain --no-legend 'fxjob-k1-*' | grep -q ."

echo "== a job's temporary directory, the job killed with SIGKILL"
# The command makes files in its TMPDIR, root's among them, mounts a tmpfs in it and writes 64M
# there, memory (shmem) charged to the job; loop-mounts an ext4 image of that tmpfs inside it and
# attaches another loop device to the image, as root test runs do; and leaves a sleep behind,
# for reap to kill, once a SIGKILL ends it. $1 is where it says what it made.
# shellcheck disable=SC2016 # For the job's shell.
mount_in_tmpdir='set -e
cd "$TMPDIR"
echo "$TMPDIR" > "$1/tmpdir"
head -c 1M /dev/urandom > file
sudo -n mkdir -m 700 root
sudo -n touch root/file
mkdir mem
sudo -n mount -t tmpfs -o size=128M,mode=1777 fxjob-check mem
head -c 64M /dev/zero > mem/zeros
truncate -s 32M mem/image
sudo -n mkfs.ext4 -q -F mem/image
mkdir mem/image.d
sudo -n mount -o loop mem/image mem/image.d
findmnt -n -o SOURCE mem/image.d > "$1/loops"
sudo -n losetup -f --show mem/image >> "$1/loops"
sleep 300 &
echo $$ > "$1/pid"
wait'
work_cgroup=/sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/fxdax.slice/$FXJOB_WORK_SLICE
memory() { cat "$work_cgroup/memory.current" 2> /dev/null || echo 0; }
shmem() { sed -n 's/^shmem //p' "$work_cgroup/memory.stat" 2> /dev/null || echo 0; }
mib() { echo $(( $1 >> 20 ))M; }
# ready FILE: waits for the job to write FILE, 30 s at most.
ready() {
  local tries
  for (( tries = 0; tries < 300; tries++ )); do
    [[ ! -s $1 ]] || return 0
    sleep 0.1
  done
  return 1
}
# leftovers DIR: what is left of the job's temporary directory DIR: itself and what is mounted
# in it.
leftovers() {
  if [[ -e $1 ]]; then echo "$1"; fi
  findmnt -rn -o TARGET | grep -F "$1" || true
}
# attached FILE: the loop devices FILE names, a line each, still attached to a file.
attached() {
  local device
  while read -r device; do
    if [[ -e /sys/block/${device#/dev/}/loop/backing_file ]]; then echo "$device"; fi
  done < "$1"
}
mkdir "$tmp/t1"
memory_before=$(memory) shmem_before=$(shmem)
"$fxjob" --name t1 --mem 256M --cpus 1 -- bash -c "$mount_in_tmpdir" t1 "$tmp/t1" \
  2> "$tmp/t1.err" &
pid[t1]=$!
check "t1 made its files and mounts" ready "$tmp/t1/pid"
dir=$(< "$tmp/t1/tmpdir")
check "t1's TMPDIR is its own, anyone's to write in, as /tmp" \
  test "$dir" == "$FXJOB_TMP/${pid[t1]}" -a "$(stat -c %a "$dir")" == 1777
check "with two mounts in it, and two loop devices on its image" \
  test "$(leftovers "$dir" | wc -l)" -eq 3 -a "$(attached "$tmp/t1/loops" | wc -l)" -eq 2
memory_running=$(memory) shmem_running=$(shmem)
check "work's slice holds t1's tmpfs: shmem $(mib "$shmem_running"), $(mib "$shmem_before") before" \
  test "$shmem_running" -ge $(( shmem_before + (64 << 20) ))
kill -KILL "$(< "$tmp/t1/pid")"
finished t1
check "t1 ended by its SIGKILL" test "${status[t1]}" -eq 137
check "nothing is left of its TMPDIR, nothing mounted there" test -z "$(leftovers "$dir")"
check "no loop device left on its image" test -z "$(attached "$tmp/t1/loops")"
memory_reaped=$(memory) shmem_reaped=$(shmem)
check "work's slice no longer holds it: shmem $(mib "$shmem_reaped"), memory $(mib "$memory_reaped") of $(mib "$memory_running") running, $(mib "$memory_before") before" \
  test "$shmem_reaped" -le $(( shmem_before + (1 << 20) )) -a "$memory_reaped" -le $(( memory_before + (8 << 20) ))
check "t1's fxjob said nothing but that its command was killed: $(paste -sd '|' "$tmp/t1.err")" \
  bash -c '! grep -qv " Killed  *\"\$@\"\$" "$1"' said "$tmp/t1.err"

echo "== a job's temporary directory, its fxjob killed outright"
mkdir "$tmp/t2"
setsid "$fxjob" --name t2 --mem 256M --cpus 1 -- bash -c "$mount_in_tmpdir" t2 "$tmp/t2" \
  2> "$tmp/t2.err" &
pid[t2]=$!
check "t2 made its files and mounts" ready "$tmp/t2/pid"
dir=$(< "$tmp/t2/tmpdir")
kill -KILL -- "-${pid[t2]}"
finished t2
check "its TMPDIR stays, no reap having run" test -d "$dir"
"$fxjob" --name t3 --mem 10M --cpus 1 -- true
check "the next job's fxjob removes it, nothing mounted there" test -z "$(leftovers "$dir")"
check "no loop device left on its image" test -z "$(attached "$tmp/t2/loops")"

echo "== a waiting fxjob killed outright gives up its place"
job q1 3 --mem 250M --cpus 1
job q2 1 --mem 200M --cpus 1
job q3 1 --mem 50M --cpus 1
kill -KILL "${pid[q2]}"
finished q1 q2 q3
check "q3 starts beside q1, q2 gone" before start q3 end q1
check "q2 never ran" test -z "$(at start q2)"
check "the queue is empty" test "$(entries)" -eq 0

echo "== a signal ends a waiting fxjob"
job s1 2 --mem 250M --cpus 1
job s2 1 --mem 250M --cpus 1
kill -TERM "${pid[s2]}"
finished s1 s2
check "s2 exits 143, never having run" test "${status[s2]}" -eq 143 -a -z "$(at start s2)"
check "the queue is empty" test "$(entries)" -eq 0

echo "== measurements"
job w1 2 --mem 100M --cpus 4
job b1 2 --bench --mem 100M
job w2 1 --mem 50M --cpus 1
job b2 1 --bench --mem 50M
finished w1 b1 w2 b2
check "b1's 8 CPUs wait for w1's 4 of 10 to end" before end w1 start b1
check "w2, which fitted beside w1, waits behind b1" order w1 b1 w2
check "w2 runs beside b1" before start w2 end b1
check "b2 waits for b1 to end" before end b1 start b2
check "b2 said it waited for b1" grep -q 'another measurement' "$tmp/b2.err"

echo "== a job inside a job skips the queue"
rc=0
"$fxjob" --name outer --mem 250M --cpus 1 -- "$fxjob" --name inner --mem 100M --cpus 1 -- true \
  2> "$tmp/outer.err" || rc=$?
check "inner, past the budget beside outer, runs" test $rc -eq 0
check "saying it didn't wait its turn" grep -q 'without waiting' "$tmp/outer.err"

echo "== status and history"
rc=0; "$fxjob" --name three --mem 10M -- bash -c 'exit 3' || rc=$?
check "fxjob exits with the command's status" test $rc -eq 3
# shellcheck disable=SC2016 # As they are.
"$fxjob" --name dollars --mem 10M -- printf '%s\n' '$HOME ${HOME} $$' > "$tmp/dollars"
# shellcheck disable=SC2016 # As they are.
check "the command gets \$VAR, \${VAR} and \$\$ as they were" \
  test "$(< "$tmp/dollars")" == '$HOME ${HOME} $$'
# A measurement here never swaps (MemorySwapMax=0), so its limit kills it rather than swapping it
# out on a host whose swap is someone else's. What it takes is bounded, 256M read into memory,
# its output nowhere: were the limit not set, it would take that and no more. (tail of /dev/zero,
# which was to grow without end, wrote the zeros out instead, and what read them, outside every
# job's limit, grew to 11.7 GB, until user-1000.slice's OOM killer took it.)
# Only with FXJOB_CHECK_OOM=1: the kernel's OOM kill, however small and contained, has the
# desktop say the system ran out of memory.
oom=${FXJOB_CHECK_OOM:-0}
if [[ $oom == 1 ]]; then
  rc=0
  "$fxjob" --name oom --bench --mem 64M -- \
    bash -c 'head -c 256M /dev/zero | python3 -c "import sys; d = sys.stdin.buffer.read()"' \
    > /dev/null 2>&1 || rc=$?
  check "a job past its memory limit exits 137" test $rc -eq 137
fi
python3 - "$FXJOB_HISTORY" > "$tmp/history" <<'EOF'
import json, sys
for line in open(sys.argv[1]):
    j = json.loads(line)
    print(j["name"], j["status"], j["mem"], j["cpus"], j["queued_s"], j["wall_s"],
          j["peak_mem"], j["peak_swap"], j["cpu_s"], str(j["bench"]).lower())
EOF
history() { awk -v n="$1" '$1 == n' "$tmp/history"; }
check "every line is JSON with every field" test -s "$tmp/history"
ran=(n1 n2 n3 n4 n5 n6 big1 big2 small c1 c2 k2 t1 t3 q1 q3 s1 w1 b1 w2 b2 inner outer three dollars)
if [[ $oom == 1 ]]; then ran+=(oom); fi
check "a line for each job that ran, and no other" \
  test "$(cut -d' ' -f1 "$tmp/history" | sort | paste -sd ' ')" == \
  "$(printf '%s\n' "${ran[@]}" | sort | paste -sd ' ')"
check "n4's says it waited" awk '$1 == "n4" && $5 > 1 { f = 1 } END { exit !f }' "$tmp/history"
check "n1's says it didn't, ran 2 s, asked 100M and 1 CPU" \
  awk '$1 == "n1" && $5 < 1 && $6 >= 2 && $3 == 104857600 && $4 == 1 { f = 1 } END { exit !f }' "$tmp/history"
check "three's status is 3" test "$(history three | cut -d' ' -f2)" == 3
if [[ $oom == 1 ]]; then
  check "oom's status is 137, its peak its limit" \
    awk '$1 == "oom" && $2 == 137 && $7 >= 60 * 2^20 && $10 == "true" { f = 1 } END { exit !f }' "$tmp/history"
fi
check "k1's and t2's, killed with their fxjobs, have none" test -z "$(history k1)$(history t2)"
check "peaks and CPU time measured" \
  awk '$7 == "null" || $8 == "null" || $9 == "null" { f = 1 } END { exit f }' "$tmp/history"

echo
if (( failures )); then
  echo "fxjob-check: $failures failed" >&2
  exit 1
fi
echo "fxjob-check: all passed"
