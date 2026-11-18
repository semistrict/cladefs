---
id: TASK-532
title: Committing a sparse shrink allocates memory for every removed logical block
status: To Do
assignee: []
created_date: '2026-11-18 12:25'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 922000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 17.

At `crates/extent_files/src/pages.rs:761`, the commit plan inserts even absent blocks into a tree set, then collects parents and loads nodes to identify extents to release. Shrinking an all-hole file to zero does this with zero data extents to free. Walk allocated subtrees and skip absent children. The allocation probe below measures this separately from file creation.

Isolated arm64 macOS validation. Create and commit an all-hole file, truncate to zero, and use a counting Rust allocator around sync. Verify zero extents before the shrink. Extra peak live allocation was 286,856 bytes for 16 MiB logical size, 1,111,880 for 64 MiB, 4,433,456 for 256 MiB, and 17,698,448 for 1 GiB. These are requested heap bytes above baseline, not process RSS.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Heap usage when syncing an all-hole shrink stays bounded by allocated tree structure. Mixed sparse files release removed extents and preserve retained data.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
