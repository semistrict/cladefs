---
id: TASK-539.5
title: Allotments by granted directory; disk.rs loses its walks
status: To Do
assignee: []
created_date: '2026-11-18 14:16'
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
- [ ] #1 footprint::allotment(dir) returns one Arc<Allotment> per granted directory for the daemon's life. Making it attaches the ledger's footprints that are in_dir it and seeds the closed stores under it (directories holding store, store.deleted or unmade) once. A footprint made later attaches by the longest in_dir match.
- [ ] #2 disk.rs no longer contains walk, recount, recounter, RECOUNT_EVERY, RECOUNT_GAP, NUDGED, nudge, writing, counting or any write_back_all call. A grep test finds no non-test write_back_all caller in cladefs.
- [ ] #3 GetTenantDisk is served without cells::blocking.
- [ ] #4 A two-tenant test shows that counting one tenant leaves the other's dirty pages dirty (TASK-538).
- [ ] #5 failed_writeback_preserves_disk_admission passes with no recount. writes_not_done_stay_counted_across_walks is rewritten for pledges.
- [ ] #6 tests/disk.rs compares tenant_disk with Σ rounded lengths (and du -sxB1 on tmpfs) and passes on ubu1, its ZFS TMPDIR included.
- [ ] #7 TASK-538, TASK-525 and TASK-520 are closed citing this task.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
