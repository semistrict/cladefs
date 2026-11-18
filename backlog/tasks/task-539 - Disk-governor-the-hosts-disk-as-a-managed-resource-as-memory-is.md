---
id: TASK-539
title: 'Disk governor: the host''s disk as a managed resource, as memory is'
status: To Do
assignee: []
created_date: '2026-11-18 14:15'
labels:
  - extentd
  - disk
dependencies: []
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
priority: high
type: feature
ordinal: 932000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The host's disk has no governor. Tenant directories are counted by walking them like du -sx every 10 s, and every walk writes back the whole process's store page cache (TASK-538, TASK-525). Counts go wrong on ZFS (TASK-355) and after a failed write-back (TASK-520). Limits are off by default, and the part cache alone keeps a fixed 4 GiB floor, which concurrent fills overrun (TASK-521) and under which it evicts everything (TASK-443.8). Nothing stops the stores filling a host. The user (2026-11-18) wants the disk managed like memory: a dedicated machine is the common case, so take all but a headroom; support a fixed upper limit; have caches evict from free space (about 95% used or under X free) to be a good neighbour; and count bytes where the stores write them. doc-22 is the design and records the decisions.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every subtask is done
- [ ] #2 doc-22 matches what was built
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
