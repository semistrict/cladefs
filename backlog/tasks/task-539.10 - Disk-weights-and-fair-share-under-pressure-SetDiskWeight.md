---
id: TASK-539.10
title: Disk weights and fair share under pressure (SetDiskWeight)
status: To Do
assignee: []
created_date: '2026-11-18 14:17'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.9
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: feature
ordinal: 942000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A tenant filling the host disk must not take every other tenant's share first. This mirrors SetMemoryWeight. (doc-22 task T9.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 rpc SetDiskWeight is added to cladefsd.proto v1, control.rs and the regenerated Go client. disk::set_weight refuses the default tenant with EINVAL/INVALID_ARGUMENT, and weights persist for later grants.
- [ ] #2 Under pressure, govern_once publishes governor::shares over the allotments on the disk. With weights 3:1, the allotment over its share is refused while the other is admitted, and both commit (test).
- [ ] #3 Out of pressure, shares are unlimited (test).
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
