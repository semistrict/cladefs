---
id: TASK-444.2.3
title: >-
  loom models for the orderings stronger than needed, whose order comes from a
  socket, an eventfd or tokio's Notify
status: To Do
assignee: []
created_date: '2026-11-15 13:57'
labels: []
dependencies: []
parent_task_id: TASK-444.2
priority: low
type: task
ordinal: 863000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-444.2's audit (docs/memory-ordering.md, 'Orderings stronger than needed') found 14 groups of Acquire/Release sites that publish nothing beyond their own value or are ordered by a lock or a system call: extentd client's closing (socket shutdown), uffd Lift::asked (eventfd), executor Jobs::running (tokio Notify), server's let_go/closing/connections, physmap chunks (the PMM lock), relief's running, and others. Their mutants to Relaxed are equivalent by argument only: no loom model runs them, as each would need a stand-in for the socket, eventfd or Notify that carries the order. AC #3 of TASK-444.2 files this gap rather than leaving it unsaid.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each listed site either has a loom model (with a stand-in for what orders it) under which its Relaxed mutant is marked equivalent in a loom-mutants patch, or is weakened to Relaxed with that model's evidence
- [ ] #2 docs/memory-ordering.md says which
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
