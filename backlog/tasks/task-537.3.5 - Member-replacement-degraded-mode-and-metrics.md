---
id: TASK-537.3.5
title: Member replacement, degraded mode and metrics
status: To Do
assignee: []
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-18T17:02:00Z'
labels:
  - durability
  - peers
dependencies:
  - TASK-537.3.4
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 935000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 5. A writer must replace lost members under the joint rule, drop to f = 0 only once the bucket holds what it told, and say loudly when degraded.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Replacement installs the tail, counts both configurations while its swap is in flight, then the new one
- [ ] #2 Drop to f = 0 only once Uploaded covers every Replicated commit; no Replicated acks during it
- [ ] #3 Degraded state in stats and metrics, eager uploads while degraded
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
