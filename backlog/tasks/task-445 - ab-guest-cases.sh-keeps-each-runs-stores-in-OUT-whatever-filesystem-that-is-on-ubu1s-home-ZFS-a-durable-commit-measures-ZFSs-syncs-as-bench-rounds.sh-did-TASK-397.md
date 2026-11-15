---
id: TASK-445
title: >-
  ab-guest-cases.sh keeps each run's stores in OUT, whatever filesystem that is:
  on ubu1's home, ZFS, a durable commit measures ZFS's syncs, as bench-rounds.sh
  did (TASK-397)
status: To Do
assignee: []
created_date: '2026-11-15 12:37'
updated_date: '2026-11-15 14:51'
labels:
  - bench
  - ubu1
dependencies: []
references:
  - scripts/ab-guest-cases.sh
  - scripts/test-e2e-job.sh
priority: medium
ordinal: 849000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
scripts/ab-guest-cases.sh runs each side's case as `CLADEFS_E2E_RESULTS="$results" fxjob --bench ... scripts/test-e2e.sh CASE`, results=OUT/CASE/SIDE-ROUND: the cases' stores and the daemon's part cache are in OUT. With OUT in ubu1's home (ZFS) a store's syncs are ZFS's: TASK-397 measured bench-fs-fxfs-guest-dax's sync-loop at 10.8/s so, 30/s with the store on the ext4 scratch disk. TASK-397 fixed scripts/bench-rounds.sh by running each case through scripts/test-e2e-job.sh (the run in the job's TMPDIR, fxjob's on the scratch disk, another filesystem than ext4 refused, results copied back without stores); ab-guest-cases.sh does the same thing by its own code, and removes what a run leaves itself (CLADEFS_AB_KEEP_DATA=1 keeps it, which test-e2e-job.sh has no way to). Both sides of an A/B share the filesystem, so its ratios are less wrong than absolute numbers, but what it compares is still two builds on ZFS. Its bucket is already test-e2e.sh's default, on tmpfs. It has no test in scripts/tests.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 ab-guest-cases.sh with OUT on a filesystem other than ext4 keeps each run's stores on ext4 (the job's temporary directory) or refuses, saying so, before any run
- [ ] #2 CLADEFS_AB_KEEP_DATA=1 still keeps a run's stores, bucket and part cache, where a person finds them
- [ ] #3 A test in scripts/tests fails without the change
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
