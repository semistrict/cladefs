---
id: TASK-537.3.2
title: 'Replica members: peer RPCs, replica logs on disk, promises and incarnations'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-18T17:02:00Z'
labels:
  - durability
  - peers
dependencies: []
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 932000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 2. A daemon must hold other hosts cells logs as a member of their configurations: append, fence, read and drop, refusing older epochs and other incarnations, bounded by the lag bound per cell.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Peer service gains Open/Append (stream), Fence, ReadLog and Drop; Append has an endpoint with no per-request deadline
- [ ] #2 Replica directory with an incarnation made at creation; logs written O_DIRECT through local_cell pages; a promise per cell synced before Fence answers
- [ ] #3 Appends below the promise, or naming another incarnation, refused; logs of configurations the head no longer names swept
- [ ] #4 Unit tests with fake links: append and read back, fence then refused append, wrong incarnation, sweep
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
