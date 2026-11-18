---
id: TASK-526
title: Seeking across a sparse guest file scales with its logical size
status: To Do
assignee: []
created_date: '2026-11-18 12:25'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 916000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 11.

In `guest/linux/fs/cladefs/super.c:949`, the extent walker advances by at most 1 MiB, including on an empty result. SEEK_HOLE at zero in a 1 TiB all-hole file performs 1,048,576 lookups to return zero while holding the shared inode lock. SEEK_DATA traverses the same holes. Return the next extent offset or end-of-file from the extent lookup. Include an operation-count assertion and check FIEMAP, which shares the walker.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 SEEK_HOLE and SEEK_DATA on a large all-hole file reach their correct results with bounded lookups independent of hole length. Mixed files and FIEMAP retain correct extent boundaries.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
