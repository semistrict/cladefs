---
id: TASK-539.16
title: >-
  A framing in a copy that the disk refuses (ENOSPC) fails its store: re-queue
  it instead
status: To Do
assignee: []
created_date: '2026-11-18 17:00'
labels:
  - extentd
  - disk
dependencies: []
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: bug
ordinal: 949000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
From TASK-539.12 (doc-22, hardening). A closed segment framed in a copy writes a new framed file beside its raw one; if the host's disk refuses it (a neighbour filling the disk faster than the governor's headroom absorbs, between its looks), the framing fails and local_cell marks the store Phase::Failed / Standing::Failed, which fails every later request until the store is reopened. The raw file is whole and still the segment's: the framing could be dropped and tried again once there is room, as a write-back's is (TASK-539.12).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test whose MemDisk refuses a framing in a copy with ENOSPC, then has room, shows the store still serving, the framing done on a later try, and the crash and sim suites passing
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
