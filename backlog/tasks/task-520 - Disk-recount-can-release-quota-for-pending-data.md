---
id: TASK-520
title: Disk recount can release quota for pending data
status: Done
assignee: []
created_date: '2026-11-18 12:24'
updated_date: '2026-11-18 16:49'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 910000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 5.

In `crates/cladefs/src/cladefsd/disk.rs:209`, failed write-back only logs. Recount replaces the previous admission estimate using the directory walk, which can miss completed writes still held dirty after failure. Preserve the conservative charge on error. Check admission after injecting a write-back failure between a completed write and recount.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A failed flush during recount preserves the charge for accepted dirty writes. Recount after recovery and concurrent admissions still respect tenant and total limits.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Gone with TASK-539.5: tenants are counted from their stores' footprints as they write; nothing walks a directory or writes back the stores' page cache to count. TASK-520's test (failed_writeback_preserves_disk_admission) and TASK-538's (cladefsd::disk::tests::counting_tenants_writes_back_nothing) pass on ubu1.
<!-- SECTION:FINAL_SUMMARY:END -->
