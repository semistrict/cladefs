---
id: TASK-480
title: >-
  A late original host's unfork after a resumed clone was removed leaves a dead
  cell for good
status: To Do
assignee: []
created_date: '2026-11-16 04:47'
labels:
  - bug
  - collector
dependencies: []
priority: low
ordinal: 880000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-16 by the second review of TASK-455 (fences kept, resumes marked). Host A runs fork_point for clone C (allocator ID), creates its Forking record and stalls before refer; store B opens C and resume_point_fork marks it resumed, refers and makes it; C is deleted and a pass removes its cell whole. A wakes: its late reference is deleted by the mark as intended, but its finish_point_fork returns Error::NoBranch (crates/extent_remote/src/branch.rs, `None => Err(Error::NoBranch)`), Remote::fork_made returns it to host_store.rs fork_in, which calls remote.unfork(child) -> delete_fork_under with Kind::Forking; finding no record it creates a deleted Fork record (`Kind::Forking { origin, .. } | Kind::Clone(origin) => (Kind::Fork(origin), false)`) and end_head writes a final head. Record::removable refuses Fork, so the removed cell comes back as a dead cell that costs a look (2 requests) every sweep, and every full pass marks its reference retired again. RemovedStayGone and testing::uncollected both accept a deleted record that stays; the spec has no unfork after a failed resume; doc-7 lists only a deletion run again as the source of such cells. The race needs A to outlive B's resume, C's deletion and a sweep. A possible shape (the reviewer's): unfork skips a branch whose record is gone and whose ID an allocator gave.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The reviewer's reproducer (left_by_a_late_host_of_a_resumed_clone extended with the host's unfork, collect_until_quiet, the clone's cell listed empty) fails before the fix and passes after
- [ ] #2 Bucket.tla models the unfork after a failed resume and an invariant or check fails on the current behaviour
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
