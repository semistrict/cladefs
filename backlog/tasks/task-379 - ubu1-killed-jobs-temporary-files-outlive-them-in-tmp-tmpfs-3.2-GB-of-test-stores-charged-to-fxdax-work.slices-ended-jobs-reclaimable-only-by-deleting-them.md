---
id: TASK-379
title: >-
  ubu1: killed jobs' temporary files outlive them in /tmp (tmpfs): 3.2 GB of
  test stores charged to fxdax-work.slice's ended jobs, reclaimable only by
  deleting them
status: Done
assignee:
  - '@claude'
created_date: '2026-11-12 10:51'
updated_date: '2026-11-15 03:35'
labels:
  - bug
  - ubu1
dependencies: []
priority: medium
type: bug
ordinal: 761000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-12 while fixing TASK-378. fxdax-work.slice held 4.0 GB with no job of its own running: 3.2 GB shmem (memory.stat) in 110 dying memcgs, ended fxjob scopes. /tmp is a 14 GB tmpfs with 3.8 GB used, most of it tempfile TempDirs of fxdax tests (/tmp/.tmpXXXXXX with store/ and bucket/, e.g. .tmpJCnhSf 1.1 GB, .tmpLAS3YT 0.9 GB, .tmpyS27AI 0.7 GB, from 2026-11-11/12) whose processes are gone: tests killed (OOM kills, timeouts, cancelled runs) never drop their TempDir. tmpfs pages are reclaimed only by swap; those jobs ran with MemorySwapMax=0 (before TASK-378), so they stay charged to work, ~3 GB of work's room beside a measurement, until someone deletes the files. TASK-331 is the same leak for scripts/test-e2e.sh's cache; TASK-316 noted /tmp near full. Structural fix to weigh: fxjob gives each job a TMPDIR of its own (on disk, not tmpfs, or removed with the job at reap), so nothing a job leaves outlives it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A job killed with SIGKILL leaves none of its temporary files on ubu1
- [x] #2 fxdax-work.slice holds no memory of ended jobs once they are reaped
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. fxjob: each job gets TMPDIR=$tmp_root/PID (tmp_root /mnt/fxdax-scratch/fxjob, ext4; /tmp/fxjob if scratch isn't mounted; FXJOB_TMP for the check), mode 1777 (jailed VMs' user writes there), made under the queue lock with a marker job-PID (fxjob's start time, the job's scope).
2. reap: after killing the scope's leftovers, clean the dir: lazily unmount every mount under it (findmnt, deepest first, sudo), detach loop devices backed by files there (sysfs backing_file), refuse to rm while any mount remains, rm -r --one-file-system (sudo if root's files). Processes that outlived the job leave the dir to a later sweep.
3. sweep: every fxjob, under the queue lock, cleans dirs whose fxjob is gone and whose scope holds no process (fxjob itself SIGKILLed, no reap ran).
4. sudo strips TMPDIR (glibc drops it for setuid programs; --preserve-env can't keep it): host_command and bench-rounds.sh pass it to root runs with sudo env TMPDIR=..., expanded inside the job.
5. fxjob-check.sh: a job making files, root's files, a tmpfs mount, a loop-mounted ext4 image under TMPDIR, SIGKILLed: no dir, mount, loop left; slice memory (shmem) back after reap; fxjob SIGKILLed outright: next job sweeps its dir.
6. Verify on ubu1 (fxjob-check, nextest workspace under fxjob), macOS nextest; push; re-run step 146 when fxjob ps is idle.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-12, the coordinator cleaned ubu1's /tmp: removed 1,516 test temp dirs (.tmp*, tmp.*, fxdax-*) older than 6 h, none open by any process nor any process's cwd; 1,035 of them root's. /tmp went from 3.7G to 2.0G used.
Three of them (.tmpKHntmR, .tmpy4Rk48, .tmprPqkTq, made 05:59 that day) each held a dead fuse.fxdax mount of a killed test ('Transport endpoint is not connected'): lazily unmounted, then removed. So a killed FUSE test leaves its mount behind as well as its files.
Suggested fix: fxjob gives each job a TMPDIR of its own, which tests make their temp dirs under; at the job's end its reap lazily unmounts any fuse.fxdax mount under it and removes it.

2026-11-13 ~10:15 EST, coordinator: ubu1's swap was full (8,187 of 8,191 MiB) with /tmp (tmpfs) at 7.1G; removed 288 more stale fxdax test dirs older than 6 h and open by no process (.tmp*, tmp.*, fxdax-*, and fuse_differential's differential.*/reference.*/script.* from root runs on 2026-11-11/12, e.g. a 4.6G .tmpkkVBEM store), lazily unmounting the 11 dead mounts left in them first. /tmp 7.1G -> 640M, swap 8.2G -> 3.0G used. Root test runs leave mounted images too, not only FUSE: the fix should unmount and remove a killed job's temp dirs (a per-job TMPDIR fxjob removes).

2026-11-14, 679737d41: fxjob gives each job TMPDIR=/mnt/fxdax-scratch/fxjob/PID (ext4, 1777, 32 bytes), cleaned at reap (loops detached, mounts lazily unmounted deepest first, removed with sudo where root's) and swept by the next fxjob if fxjob itself was killed. sudo can't keep TMPDIR (glibc drops it for set-user-ID programs, --preserve-env too): host_command and bench-rounds.sh pass it with sudo env TMPDIR=..., expanded inside the job.
Evidence (ubu1): scripts/fxjob-check.sh all passed: SIGKILLed job with files, root's files, tmpfs (64M), loop-mounted ext4 inside it and a second loop: no dir, mount or loop left; check slice shmem 65M -> 0M, memory.current 68M -> 0M. fxjob killed outright: dir kept until the next fxjob, then gone. On the real fxdax-work.slice (worktree fxjob, beside t407's TLA job): a job with 512M in a tmpfs in its TMPDIR plus 256M on disk, SIGKILLed: shmem 2044M -> 2566M -> 2059M, memory.current 10705M -> 11496M -> 10722M; dir and mount gone. A real nextest run SIGTERMed by its 15-min cap (FUSE tests among those killed) left /mnt/fxdax-scratch/fxjob empty, no mounts.
DoD #1: macOS workspace 2369 passed. ubu1 under the new fxjob (tmp on ext4): 1365/2651 passed, 0 failed, then the 900 s hard timeout cut it (full suite ~30 min): the rest not run, by the 15-min heavy-load rule. Found TASK-408 (systemd-run expanded $$ in job commands), fixed in the same commit.
Note: fxdax-work.slice held 2044M shmem before any of this with only t407's job running: probably old fxjob's leftovers (tmpfs files of ended jobs); not touched.

2026-11-14 13:30 EST: queue idle; ran ubu1's step 146 on its own (sudo system-setup.d/146-fxdax_cgroups.sh, not system-setup.sh, which would also run others' pending steps: vscode, cliproxy, fxdax_tla, user_tweaks). /usr/local/bin/fxjob = origin/main 2eda3972 (md5 02aab1a0...; the step said replaced nothing: it was already that). fxjob-check.sh against the installed fxjob: all passed. Remaining for DoD #1: the full Linux workspace suite under the new fxjob (~30 min; cut at 1365/2651, 0 failed).

2026-11-14 13:35 EST, coordinator: ubu1 /tmp held a 2.2G /tmp/fxdax-tla.CnSEeW (BucketForks states, 12:17, no TLC running), scripts/check-tla.sh's work dir: its EXIT trap does not run when the script is killed. Removed; /tmp 2.8G -> 698M, swap 6.9G -> 5.1G. Covered by this task when TLC runs under fxjob (its TMPDIR is reaped); TLC runs on the Mac now anyway.

2026-11-14 (claude, TASK-400/401's agent): the full Linux workspace suite under the new fxjob, ubu1, commit dddfea15e (TASK-400/401 on origin/main 8391d2e3d), scripts/test.sh's workspace part (UCD_ZIP, sims excluded as there), --test-threads 4, fxjob --name t400-suite --mem 12G: 2655 run, 2653 passed, 2 failed, in 553 s (build 1m45s before it). Neither is fxjob's: durable_commits_keep_the_open_segments_pages_cached_on_the_host because the target (its CARGO_TARGET_TMPDIR) was on /home's ZFS (TASK-309), passing with the target on /mnt/fxdax-scratch's ext4; images_dropped_as_their_pass_runs_leave_their_bucket_whole is TASK-328's flake (1 of 3 reruns failed alone). No job left anything in /mnt/fxdax-scratch/fxjob.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
fxjob gives each job a TMPDIR of its own on /mnt/fxdax-scratch (ext4, not tmpfs, so nothing a job leaves is charged to memory); its reap unmounts every mount under it (fuse and loop), detaches loop devices left on its files and removes it, root's files too; an fxjob killed outright leaves its dir for the next fxjob to remove. Root runs (host.sh, bench-rounds.sh) pass it with sudo env TMPDIR=, since sudo drops TMPDIR (679737d41; TASK-408 fixed alongside). Verified: fxjob-check.sh on ubu1 (SIGKILLed job with files, root files, tmpfs, loop-mounted ext4 and a loop device leaves nothing; a timed-out real test run leaves nothing; slice shmem 65M -> 0M after reap), ubu1 workspace under the new fxjob 2653/2655 (TASK-309's ZFS page-cache test and TASK-328, neither this change), main's CI green on Linux and macOS (efa48fa88), upstream-changes.py --check.
<!-- SECTION:FINAL_SUMMARY:END -->
