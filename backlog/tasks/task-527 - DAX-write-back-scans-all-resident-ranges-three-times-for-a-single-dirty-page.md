---
id: TASK-527
title: DAX write-back scans all resident ranges three times for a single dirty page
status: To Do
assignee: []
created_date: '2026-11-18 12:25'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 917000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 12.

In `guest/linux/fs/cladefs/dax.c:2377`, write-back counts dirty ranges across the entire array, repeats the count under the exclusive semaphore at line 2397, then walks the array again at line 2405. Index dirty ranges and retain the exclusive recheck. Measure one dirty page as clean mapped ranges increase.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Writing back one dirty page avoids traversing every clean resident range. Concurrent dirtying during collection remains covered by the sync contract.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
