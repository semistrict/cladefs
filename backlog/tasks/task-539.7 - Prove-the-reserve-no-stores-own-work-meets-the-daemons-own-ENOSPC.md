---
id: TASK-539.7
title: 'Prove the reserve: no store''s own work meets the daemon''s own ENOSPC'
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
type: spike
ordinal: 939000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
RESERVE_TIMES is the keystone of admission. It must be measured, not assumed. (doc-22 task T12.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A sim-world property test records, per store, the largest unrefusable growth after any step and asserts it ≤ RESERVE_TIMES × unframed at that step. Its seeds cover held runs, commit copies, framing in place and in a copy, own extents and recovery.
- [ ] #2 With MemDisk::with_capacity just above what admission allows, guests write to refusal over many seeds: every ENOSPC the MemDisk returned was on a guest-admitted path, never on commit, framing, flush or acknowledgement.
- [ ] #3 If the bound fails, RESERVE_TIMES and the design's argument are corrected in the same change.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
