---
id: TASK-532
title: Committing a sparse shrink allocates memory for every removed logical block
status: In Progress
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
- [x] #1 Heap usage when syncing an all-hole shrink stays bounded by allocated tree structure. Mixed sparse files release removed extents and preserve retained data.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
The allocation probe in the storage audit found that byte comparisons alone missed work proportional to the discarded logical size. The cladefs regression measures incremental peak allocation at two all-hole sizes and reopens both files. The planner walks allocated descendants intersecting the cut and rebuilds surviving paths for discarded data, including shrink followed by regrowth. The same logical-range enumeration existed in the delete and diagnostic extent inventory. Both now use the allocated-tree traversal.
An independent macOS probe wrote 600 pages plus one at 1 GiB, shrank to 282 pages and one byte, reopened, and verified every retained byte and device extent.

The unchanged regression failed in the macOS audit run and passed against the implementation. Both implementation reviews reported no findings.
The regression also passed on ubu1 in run r516e against 24c6081a6 with the production patch. All 19 extent_files tests passed on both platforms. The macOS crate run passed 412 of 413 (TASK-530 failed), and its workspace run stopped on TASK-521. The ubu1 crate run passed 681 of 684 (TASK-519, TASK-520 and TASK-522 failed, as in the baseline audit). Workspace qualification remains open.
<!-- SECTION:NOTES:END -->
