---
id: TASK-584
title: 'Punching holes records an entry per page, even over chunks already holes'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 15:29'
updated_date: '2026-11-18 22:36'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 975000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of 6f4dd56a0 (2026-11-18), file-sets review; verified against the code by independent reviewers. Severity: medium.

**Where:** `crates/extent_files/src/pages.rs`: `rg -n -F 'self.dirty.insert(index, None);' crates/extent_files/src/pages.rs`

Pages::punch inserts a `None` into `dirty` for every chunk in the range, whether or not the last commit already held a hole there. plan then clones that map into `written` and pushes every one of those chunks through `placed_places`, `left` (a clone of it), the `parents` Vec, the `placed` BTreeMap and `changed`. That is about 170 bytes and several B-tree inserts per punched page, for chunks whose commit changes nothing.

**Trigger.** MemoryCell::open of a new memory: truncate(size) + sync commits an all-hole file. MemoryCell::import then punches every zero page of the snapshot file (runs_of_zeros -> punch per run -> Pages::punch per page). A 64 GiB VM image that is 90% zeros puts about 15M entries into dirty, and the single commit builds several more 15M-entry structures from them: GBs of heap and seconds of B-tree work, all for chunks that were already holes and store nothing.

**Why the tests stay green.** The memory-cell tests punch 1-30 pages, and pages.rs's random test punches at most 40 chunks. No test punches a large range or measures allocation; TASK-532's probe covers only shrinking.

**Backlog.** No related task covers it: TASK-532 (To Do) fixes only the shrink path's per-block entries and leaves punch's; TASK-456.4 (To Do) will punch a VM's zero pages in one commit and TASK-456.5 (To Do) plans 64 GiB VMs; TASK-524 is local_cell's pages.rs.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Punching a range the last commit already held as holes leaves nothing to commit: changed() is false and sync allocates O(tree nodes), not O(chunks), for a 1 GiB all-hole file punched whole.
- [ ] #2 Punching 1 GiB over committed data keeps the commit's peak heap within the written data plus O(tree nodes): punched ranges are not kept as one entry per page, and a memory cell's import of a mostly-zero VM snapshot's memory file stays within that bound.
- [ ] #3 A counting-allocator regression test in extent_files' pages.rs covers both cases and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18: taken up by the Codebase Bug and Performance Audit session, in one batch of file-set fixes with TASK-551, 552, 553, 530, 583 and 584 (branch t3/codebase-bug-perf-review).
<!-- SECTION:NOTES:END -->
