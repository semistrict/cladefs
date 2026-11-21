---
id: TASK-704
title: Shredding a tenant or cell does not halt an open ephemeral cell store
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 20:14'
updated_date: '2026-11-21 05:32'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
priority: high
ordinal: 1109000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Split from TASK-698.7 at the close of TASK-698 (2026-11-20). An open ephemeral store now holds its ring among the host keys (OpenCell.keys, a RingHold), so forgetting a shredded tenant or cell drops it from the host fetchers, and reads of what the cell inherits fail. But nothing halts the store itself: it keeps reading and writing its own segments under its own DEK, in memory, until closed, where a bucket cell store is halted (crate::halt) by the forget path (TASK-637, TASK-562 for the FUSE mount).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Forgetting the tenant or cell of an open ephemeral cell halts its store (EACCES), as a bucket cell store is halted
- [x] #2 A test shreds the tenant of an open ephemeral cell and sees its next read and write refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 scripts/upstream-changes.py --check passes
- [x] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
No code change needed: an ephemeral cell's OpenCell is assembled through OpenCell::assemble like any other, which lists its Halt with its cell, so crate::bucket::forget (ShredCell, ForgetTenantKeys) already halts it with EACCES. The task's premise was wrong. Added cladefsd::tests::forgetting_keys_halts_open_ephemeral_cells: two ephemeral cells of one version open; forgetting the first's cell keys refuses its reads (own and inherited), writes and commits with EACCES while the second reads on; forgetting the tenant's halts the second (and the version's store the daemon holds). Passes on ubu1 with the other ephemeral/forget/shred tests (6/6). No upstream code touched.
<!-- SECTION:FINAL_SUMMARY:END -->
