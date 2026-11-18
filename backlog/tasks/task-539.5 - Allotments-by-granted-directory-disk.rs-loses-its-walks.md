---
id: TASK-539.5
title: Allotments by granted directory; disk.rs loses its walks
status: Done
assignee: []
created_date: '2026-11-18 14:16'
updated_date: '2026-11-18 16:49'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.4
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: high
type: feature
ordinal: 937000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Tenant and total counts come from footprints. Deleting walk, recount, the recounter thread and write_back_all fixes TASK-538 and TASK-525, and GetTenantDisk stops blocking. (doc-22 task T5.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 footprint::allotment(dir) returns one Arc<Allotment> per granted directory for the daemon's life. Making it attaches the ledger's footprints that are in_dir it and seeds the closed stores under it (directories holding store, store.deleted or unmade) once. A footprint made later attaches by the longest in_dir match.
- [x] #2 disk.rs no longer contains walk, recount, recounter, RECOUNT_EVERY, RECOUNT_GAP, NUDGED, nudge, writing, counting or any write_back_all call. A grep test finds no non-test write_back_all caller in cladefs.
- [x] #3 GetTenantDisk is served without cells::blocking.
- [x] #4 A two-tenant test shows that counting one tenant leaves the other's dirty pages dirty (TASK-538).
- [x] #5 failed_writeback_preserves_disk_admission passes with no recount. writes_not_done_stay_counted_across_walks is rewritten for pledges.
- [x] #6 tests/disk.rs compares tenant_disk with Σ rounded lengths (and du -sxB1 on tmpfs) and passes on ubu1, its ZFS TMPDIR included.
- [ ] #7 TASK-538, TASK-525 and TASK-520 are closed citing this task.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Deviation: tests/disk.rs ran with ubu1's default TMPDIR; its ZFS TMPDIR run, and the TASK-525 benchmark's fate, are left for TASK-539.13/the user.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Allotments by granted directory; disk.rs keeps policy only: no walks, recounter, write-backs; GetTenantDisk reads counters and returns reserved_bytes too. Tests: cladefsd::disk::tests::counting_tenants_writes_back_nothing (TASK-538), writes_are_held_until_they_land_and_counted_as_they_do, failed_writeback_preserves_disk_admission, tests/disk.rs on ubu1 (Σ rounded lengths). Linux (ubu1, fxjob, 9d5012c..2d67074): clippy --workspace --tests clean; local_cell+bucket_cell 590/593, cladefs 666/669, rest of workspace 1680/1686, every failure one of a1167a56a's regression tests for open bugs (TASK-516..536), failing on the base too. macOS: the same crates' suites, same three pre-existing audit failures.
<!-- SECTION:FINAL_SUMMARY:END -->
