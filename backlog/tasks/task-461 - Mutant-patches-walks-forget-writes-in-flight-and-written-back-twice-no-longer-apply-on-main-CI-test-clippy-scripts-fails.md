---
id: TASK-461
title: >-
  Mutant patches walks-forget-writes-in-flight and written-back-twice no longer
  apply on main: CI test (clippy scripts) fails
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 14:26'
updated_date: '2026-11-15 14:37'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 869000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main CI run 43722206629 (9d814644b) failed test (clippy scripts) on its mutant checks: crates/cladefs/mutants/walks-forget-writes-in-flight.patch (crates/cladefs/src/extentd/disk.rs:202) and written-back-twice.patch (crates/extent_store/src/dir.rs:460) do not apply. Both applied at ab1adf06b (the rename); the files changed since in d2cfd3b4c (t3/tombstones merged into main) and 3139c4bdd (TASK-433.1/433.3, extent_store::pages), which did not update the patches.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Both patches apply to main and still produce the mutant they name, each killed by its test
- [x] #2 The scripts part of scripts/test.sh passes on main
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Re-anchor walks-forget-writes-in-flight.patch on Disk::recount as it is (write_back_all of the stores' page cache now before the walk): the same mutation, delta without the writes not done.
2. Re-anchor written-back-twice.patch on HostFile::write_back, now a dispatch to the disk or the page cache (extent_store::pages): once written back, the range read and written again past the cache to the file on the disk, the same bytes reaching the disk twice.
3. ubu1 under fxjob: mutants.sh with both (killed), the killers without (pass); the scripts part of scripts/test.sh on the Mac.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Both patches regenerated from the current code with their headers kept (written-back-twice's says where it now writes again). ubu1 (fxjob, 07acd7087 plus the patches): mutants.sh walks-forget-writes-in-flight written-back-twice: killed both, the first at disk.rs:433 (left 4096, right 1052672), the second by 'the kernel wrote 2263 MiB for the store, which says 783 MiB in 5109 writes: 803 MiB at most'; without them both killers pass (writes_not_done_stay_counted_across_walks, a_hosts_store_writes_what_its_design_says). CLADEFS_TEST_PARTS=scripts scripts/test.sh on the Mac: exit 0, 350 tests OK, mutants 40, 4, 22, 2, 58 checked, 0 broken. scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->
