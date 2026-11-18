---
id: TASK-523
title: A failed flag change can affect the live inode
status: To Do
assignee: []
created_date: '2026-11-18 12:24'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 913000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 8.

In `guest/linux/fs/cladefs/super.c:926`, the live flags and ctime change before persistence returns its result. Setting immutable can therefore fail while subsequent writes are refused. Check GETFLAGS and write access after a failed attribute write and after inode eviction.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 After a failed immutable-flag update, GETFLAGS and write access agree with the pre-call state both before and after inode eviction.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
