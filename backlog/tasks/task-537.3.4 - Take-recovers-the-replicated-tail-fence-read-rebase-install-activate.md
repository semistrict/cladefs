---
id: TASK-537.3.4
title: 'Take recovers the replicated tail: fence, read, rebase, install, activate'
status: To Do
assignee: []
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-18T17:02:00Z'
labels:
  - durability
  - peers
dependencies:
  - TASK-537.3.3
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 934000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 4. A take on any host must recover every Replicated commit: fence f+1 members other than the old writer, read the longest log, check its MACs, rebase it (Wal::rebase accepting a tail after last_commit), install the new configuration and activate it by the head CAS before serving.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Take fences f+1 non-writer members (the old writers host counted only when it is the taker) and recovers the longest log
- [ ] #2 Wal::rebase accepts a log whose first commit follows last_commit
- [ ] #3 New configuration installed on f+1 members and activated before the store serves
- [ ] #4 Tests: writer host lost, its Replicated commits recovered on another host; a zombie writer refused after the fence
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
