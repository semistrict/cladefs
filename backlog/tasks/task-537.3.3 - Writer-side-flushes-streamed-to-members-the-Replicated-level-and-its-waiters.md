---
id: TASK-537.3.3
title: 'Writer side: flushes streamed to members, the Replicated level and its waiters'
status: To Do
assignee: []
created_date: '2026-11-18T17:02:00Z'
updated_date: '2026-11-18T17:02:00Z'
labels:
  - durability
  - peers
dependencies:
  - TASK-537.3.1
  - TASK-537.3.2
documentation:
  - >-
    backlog/docs/doc-21 -
    Replicated-commits-a-majority-of-hosts-disks-before-the-bucket.md
parent_task_id: TASK-537.3
priority: high
type: feature
ordinal: 933000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-21 step 3. The writer must send each flush records (Held) to its members, keep their ends, and release Replicated waiters on f+1 copies or Uploaded; the lag bound must cover every commit of a replicated cell.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each flush sends its records to every member at once with the local sync; members ends tracked
- [ ] #2 Replicated waiters woken at f+1 copies (the writers own counting) or at Uploaded; none on f = 0
- [ ] #3 fsync level per cell honoured in commit_and_wait; sync family waits for Replicated in a replicated cell
- [ ] #4 Tests: commits released by member acks with the bucket stalled; released by Uploaded with members down
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
