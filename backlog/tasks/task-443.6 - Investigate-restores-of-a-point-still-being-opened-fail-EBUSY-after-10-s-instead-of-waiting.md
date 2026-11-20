---
id: TASK-443.6
title: >-
  Investigate: restores of a point still being opened fail EBUSY after 10 s
  instead of waiting
status: Done
assignee: []
created_date: '2026-11-15 12:25'
updated_date: '2026-11-20 06:50'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: medium
ordinal: 840000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/cladefs/src/cladefsd/stores.rs: the first restore of a point holds its key through take_store, a full open; every other restore waits in hold_to_fork with CLOSING_WAIT (10 s) and returns EBUSY; fork_from retries only Opened::Before. 200 restores of one snapshot on a cold host with a slow bucket or a saturated disk would fail in a burst. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 20 concurrent ForkFrom opens of one cold point with the bucket slowed so its open exceeds 10 s: the count of EBUSY is recorded
- [x] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Seen 2026-11-20 on ubu1 (load ~15 on 16 cores) running cladefsd::tests at --test-threads 32 under fxjob, at main 05299a254 plus a test-only change (t3/task-677): a_zygotes_restores_open_its_copy_once failed 3 times in 4 such runs: restore 0/4/8 Err(EBUSY) at tests.rs:5247 (the restore of a version another restore is still opening gives up with EBUSY). Passes alone (5-7 s).

Measured (ubu1, 2026-11-20): 20 restores at once of one version, the bucket 600 ms away (CLADEFS_BUCKET_LATENCY_MS), the version's opening taking ~10.9 s: 11 of 20 failed EBUSY at 10.0 s. 3 at once: none failed (each waits less: the waits add up as restores take the version's store in turn after the opening). A customer would feel it restoring ~10 or more VMs of one snapshot at once on a cold host whose bucket or disk makes the version's opening near 10 s (seen at 50 ms latency under heavy load in tests: the opening ~1 s alone). Fixed here rather than filed apart (one constant's use, its test): VERSION_WAIT, holds of a version's store wait as long as the holder holds it, its holders being the daemon's own work (openings, syncs, the idle close). Test restores_wait_for_a_version_however_long_its_opening_takes: red without the fix (EBUSY), green with it (42 s).

Verified: Mac workspace 2814/2814; ubu1 under fxjob clippy + workspace 3127/3127; upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Measured 11 of 20 concurrent restores failing EBUSY when a version's opening passes 10 s; restores of a version now wait for its holder (the daemon's own work) as long as it holds it. Regression test red without, green with; Mac 2814/2814, ubu1 3127/3127.
<!-- SECTION:FINAL_SUMMARY:END -->
