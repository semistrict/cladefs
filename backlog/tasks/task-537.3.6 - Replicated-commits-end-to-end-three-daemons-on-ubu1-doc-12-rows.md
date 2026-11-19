---
id: TASK-537.3.6
title: 'Replicated commits end to end: three daemons on ubu1, doc-12 rows'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-19T02:44:00Z'
labels:
  - durability
  - peers
dependencies:
  - TASK-537.3.5
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 936000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 6. The guarantee must be shown on real daemons, including the liveness the spec leaves out.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Three daemons: the writers daemon and disk killed mid-workload, every Replicated commit present after a take elsewhere
- [ ] #2 A member down: commits stay Replicated after replacement; both down: released by Uploaded
- [ ] #3 doc-12 gains the Replicated level and its failure rows with their tests
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
