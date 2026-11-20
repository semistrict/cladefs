---
id: TASK-704
title: Shredding a tenant or cell does not halt an open ephemeral cell store
status: To Do
assignee: []
created_date: '2026-11-20 20:14'
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
- [ ] #1 Forgetting the tenant or cell of an open ephemeral cell halts its store (EACCES), as a bucket cell store is halted
- [ ] #2 A test shreds the tenant of an open ephemeral cell and sees its next read and write refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
