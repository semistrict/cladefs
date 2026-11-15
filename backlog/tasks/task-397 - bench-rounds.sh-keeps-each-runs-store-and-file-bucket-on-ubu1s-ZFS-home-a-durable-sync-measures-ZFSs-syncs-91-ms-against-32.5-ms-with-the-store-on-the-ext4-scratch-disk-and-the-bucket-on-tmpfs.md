---
id: TASK-397
title: >-
  bench-rounds.sh keeps each run's store and file bucket on ubu1's ZFS home: a
  durable sync measures ZFS's syncs, 91 ms against 32.5 ms with the store on the
  ext4 scratch disk and the bucket on tmpfs
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-13 10:20'
updated_date: '2026-11-15 14:49'
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
- [ ] #1 bench-rounds.sh with no overrides puts the store on /mnt/fxdax-scratch and the file bucket on tmpfs (or a documented equivalent), and the sync-loop on ubu1 at 25 ms reads within 10% of 30/s
- [ ] #2 Earlier ubu1 measurements that ran with the store on ZFS home are noted where they're cited (doc-12's lag figures, TASK-322)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
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
<!-- SECTION:NOTES:END -->
