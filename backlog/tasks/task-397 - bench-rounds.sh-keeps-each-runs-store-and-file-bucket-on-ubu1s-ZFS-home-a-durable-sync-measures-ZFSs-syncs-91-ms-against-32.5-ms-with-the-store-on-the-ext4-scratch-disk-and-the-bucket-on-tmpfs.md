---
id: TASK-397
title: >-
  bench-rounds.sh keeps each run's store and file bucket on ubu1's ZFS home: a
  durable sync measures ZFS's syncs, 91 ms against 32.5 ms with the store on the
  ext4 scratch disk and the bucket on tmpfs
status: Done
assignee:
  - '@claude'
created_date: '2026-11-13 10:20'
updated_date: '2026-11-16 00:43'
labels:
  - bench
  - ubu1
dependencies: []
priority: medium
ordinal: 781000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-371's AC #4 on ubu1, main 10f613290, CLADEFS_REMOTE_LATENCY_MS=25: bench-fs-fxfs-guest-dax's sync-loop runs 10.8/s with bench-rounds.sh's default (results dir, so the store and CLADEFS_EXTENTD_REMOTE=file://$out/bucket, under ~/src on ZFS home), 15.9/s with the bucket on /dev/shm, and 30.1/s with the store on /mnt/fxdax-scratch and the bucket on /dev/shm (one bucket round trip, ~30 ms, a sync). The file bucket's create and the store's syncs on ZFS home add ~60 ms a sync: what the measurement then says is ZFS's, not cladefs's or object storage's (a remote bucket's cost is the injected latency). Make bench-rounds.sh's default the representative layout: the run's store on the scratch disk (ext4, as a host's local disk) and its file bucket on tmpfs (the latency injected stands for the remote), results copied back; or refuse a store on ZFS. The earlier 3/s that looked like a borked machine was partly this.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 bench-rounds.sh with no overrides puts the store on /mnt/fxdax-scratch and the file bucket on tmpfs (or a documented equivalent), and the sync-loop on ubu1 at 25 ms reads within 10% of 17.5/s: since TASK-407 a durable commit is two bucket requests by design (the segment's create, then the head's swap; measured in TASK-407.5), so about 17.5/s, not the 30/s of one round trip before it
- [x] #2 Earlier ubu1 measurements that ran with the store on ZFS home are noted where they're cited (doc-12's lag figures, TASK-322)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. scripts/test-e2e-job.sh RESULTS [ARGS] (new): runs test-e2e.sh with its results directory, so the cases' stores and the daemon's part cache, in the job's TMPDIR (fxjob's, /mnt/fxdax-scratch/fxjob/PID on ubu1: derived, not named), refuses (exit 2) a TMPDIR unset or not on ext4 (as sandboxes.sh refuses its disk), then copies what the run left to RESULTS but for stores, caches, root filesystems and images.
2. scripts/bench-rounds.sh: each run through test-e2e-job.sh; no bucket of its own any more: test-e2e.sh's default, a file bucket on tmpfs (CLADEFS_EXTENTD_REMOTE still names another); a run refused (exit 2) stops the rounds at once. Header brought up to date (the bucket now counts against CLADEFS_ROUNDS_MEM again).
3. Tests: scripts/tests/test_e2e_job.py, fake findmnt and test-e2e.sh: where the run wrote, what came back, the refusals, the status; bench-rounds.sh's wiring by its text.
4. ubu1: worktree of branch t397, cached guest kernel, bench-rounds.sh with no overrides, bench-fs-fxfs-guest-dax, CLADEFS_BENCH_GROUPS=sync, CLADEFS_REMOTE_LATENCY_MS=25, one round: sync-loop within 10% of 30/s; record where store and bucket were.
5. Other scripts: bench.sh/bench-ab/qualify/gc-sweep through host.sh keep results and work under CLADEFS_HOST_SCRATCH (ext4) and the default bucket on tmpfs: no change; ab-guest-cases.sh keeps stores in OUT: task filed.
6. AC #2: note in doc-12, TASK-322, and where bench-rounds' ZFS-home runs are cited (docs/measurements READMEs, TASK-371, TASK-350).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15: scripts/test-e2e-job.sh (new) and bench-rounds.sh through it, committed 5dbac3d11 on branch t397. scripts/tests/test_e2e_job.py (9 tests: where the run wrote, what comes back, the refusals of zfs, tmpfs, no findmnt answer and no TMPDIR, the status; bench-rounds.sh's wiring by its text, as it runs only on a host with fxjob, as root); the scripts' suite 326 green on macOS, shellcheck clean, license headers and exclusions green.
Other scripts checked: bench.sh under bench-host.sh and bench-ab-host.sh, qualify.sh under the A/B, gc-sweep.sh under gc-sweep-host.sh all write under the run's directory, FXDAX_HOST_SCRATCH (/mnt/fxdax-scratch/fxdax-host, ext4), with test-e2e.sh's default bucket on tmpfs; sandboxes.sh refuses a disk that isn't ext4 already. ab-guest-cases.sh keeps stores in OUT: TASK-445. Found on the way: bench-rounds.sh's build drops the caller's CARGO_BUILD_JOBS and CARGO_TARGET_DIR (sudo): TASK-446.
AC #2: doc-12 notes that none of its figures ran on ZFS home (GCE's; TASK-384's on ext4 scratch with the bucket on tmpfs) and which did (bench-rounds' before today); TASK-322 and TASK-371 noted; the three READMEs of bench-rounds' ZFS-home runs (docs/measurements/ubu1-loop-baseline-2026-11-11, -governed, ubu1-unsafe-2026-11-11) carry a note. Decisions in OPEN_QUESTIONS.md.

2026-11-15, ubu1, bench-rounds.sh with no override of where a run writes: RESULTS in the home (ZFS), ~/src/fxdax-t397-results; `FXDAX_GUEST_KERNEL=<cached release-2422e3e3, built from 95d150a2a, whose kernel sources differ from this tree's by a test's comment> FXDAX_BENCH_GROUPS=sync FXDAX_REMOTE_LATENCY_MS=25 FXDAX_BENCH_ITERATIONS=1 scripts/bench-rounds.sh ~/src/fxdax-t397-results 1 bench-fs-fxfs-guest-dax`, tree da0b78edd (branch t397 = main 836047da0 and this change), artifacts built beforehand at 4 jobs so that its own build compiled nothing (TASK-446). Its dropping of the host's caches was left out (a sudo in front of the real one answering that one command), as others' jobs shared the host: the one difference from the script as it is.
- Where the run wrote: the store at /mnt/fxdax-scratch/fxjob/1523765/fxdax-e2e.p3VPc6/bench-fs-fxfs-guest-dax/bench-fs-fxfs-guest-dax/extents (firecracker.json's path_on_host; ext4), no FXDAX_EXTENTD_REMOTE given (run.json), so test-e2e.sh's bucket, /dev/shm/fxdax-e2e-bucket.*; results back in RESULTS/bench-fs-fxfs-guest-dax/round-1/e2e, 99 KB (logs, stats, JSON; no rootfs, image or store: 505 MB before).
- main (da0b78edd): sync-loop 17.5/s, syncfs-loop 16.8/s, freeze-loop 5.6/s (runs.tsv: ok, stalled cpu 24 ms, io 281 ms). Not 30/s: since TASK-407 a durable commit is two bucket requests, the segment's create then the head's swap (object-store create=262 at 26.1 ms, update=257 at 26.3 ms), 52.4 of a sync's 57.0 ms; docs/measurements/durable-commits-under-the-head-2026-11-15.md says the same of the daemon alone, about 30/s to about 17/s. What is local is 4.6 ms a sync, against ZFS home's ~60.
- The tree before TASK-407 (5632ac7a9, with this change's two scripts; same kernel, its sources identical), same command, RESULTS ~/src/fxdax-t397-results-before: sync-loop 25.8/s, syncfs-loop 32.2/s, freeze-loop 8.2/s (create=263 at 26.3 ms, update=2), one iteration, the host's load 10 before and 472 after (another agent's stress-ng beside it; io stall 675 ms). TASK-371's reference on an idle host: 30.1, 30.8, 9.9.
A first run failed for want of FXDAX_BENCH_ITERATIONS (the case skips without it; bench.sh sets it, bench-rounds.sh passes the caller's): its log named the job's directory, gone by then, so test-e2e-job.sh now says where the results went (da0b78edd).

The tree before TASK-407 again, FXDAX_BENCH_ITERATIONS=3 (RESULTS ~/src/fxdax-t397-results-before2, 14:05Z, load 19 before): sync-loop 24.3, 33.1, 33.2/s; syncfs-loop 26.7, 32.9, 33.5/s; freeze-loop 8.8, 10.5, 9.4/s; a durable commit 29.6 ms at the device once warm (40.7 ms in the first iteration), create=773 at 26.2 ms, update=2. So with this layout a sync is the bucket's round trips and 3-5 ms: one round trip before TASK-407 (33/s warm), two on main (17.5/s, its one iteration the cold one).
AC #1: the layout half is met and shown (store on /mnt/fxdax-scratch through fxjob's TMPDIR, bucket on /dev/shm, results back in RESULTS). Its number, within 10% of 30/s, is not met on main: 17.5/s, TASK-407's second request, not the layout's; left unchecked for the user's word (OPEN_QUESTIONS.md). AC #2 met: doc-12, TASK-322, TASK-371, three READMEs.
DoD #1 not run: the change is scripts and notes alone (scripts' suite 326 green on macOS, shellcheck clean; Linux: the ubu1 runs above ran both scripts as they are). #2 scripts/upstream-changes.py --check passes. #3: no upstream code touched.
ubu1 left as found: worktree, results, scratch directory removed; fxjob ps showed none of mine. /dev/shm holds 7 empty fxdax-e2e-bucket.* of 2026-11-10 to 11-13, others' (TASK-331, noted there).

Correction: the tree measured as main, da0b78edd, is main 836047da0, this change, and six commits another agent made on this branch meanwhile (15b90ab05..cdd40860e: test-only benchmarks in fxdax and extent_store behind cfg(test) and the testing feature, fxdax-gc-fill, gc-sweep.sh; no kernel source, nothing a sync runs).

2026-11-15, the user's decision: AC #1's 30/s is stale. Since TASK-407 a durable commit is two bucket requests by design (the segment's create, then the head's swap; measured in TASK-407.5), so the sync-loop on ubu1 at 25 ms reads about 17.5/s on main, not 30/s. AC #1 reworded to within 10% of 17.5/s, naming TASK-407, and checked on the run above: 17.5/s on main da0b78edd, the store on /mnt/fxdax-scratch through fxjob's TMPDIR, the bucket on /dev/shm. OPEN_QUESTIONS.md's entry says it was decided.
DoD #1 left unchecked: cargo nextest run --workspace on Linux (ubu1, under fxjob) has not run for this change (macOS: the scripts' suite green, 2026-11-15, on the branch rebased onto main 76fabab76). So the task stays In Progress until a ubu1 workspace run passes; then check DoD #1 and set it Done.

2026-11-15: Linux, ubu1 under fxjob (CARGO_BUILD_JOBS=4, --test-threads 4), main 21f042594: scripts/test.sh parts workspace and go, 2817 tests passed, 83 skipped; go/extentd/v1 ok (clippy passed in the run before, on 618403778). macOS: the crates this change touched and the scripts suite (see notes above). DoD #1 checked on that evidence.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
bench-rounds.sh runs each run through the new scripts/test-e2e-job.sh, in the job's TMPDIR on ubu1's ext4 scratch disk (refusing another filesystem than ext4), its bucket test-e2e.sh's default on tmpfs, results copied back without stores, caches and images; before, each run's store and file bucket were on ubu1's ZFS home, whose syncs a durable sync measured (91 ms against 32.5). Earlier citations of ZFS-home runs noted (doc-12, TASK-322, TASK-371, three measurement READMEs). Verified: scripts/tests/test_e2e_job.py and the scripts' suite on macOS; on ubu1, bench-rounds.sh with no overrides wrote its store on /mnt/fxdax-scratch and its bucket on /dev/shm, the sync-loop 17.5/s at 25 ms on main (two bucket requests a durable commit since TASK-407; 33/s warm on the tree before it). Open: DoD #1, the workspace's tests on Linux.
<!-- SECTION:FINAL_SUMMARY:END -->
