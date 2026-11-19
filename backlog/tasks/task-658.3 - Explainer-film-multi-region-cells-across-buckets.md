---
id: TASK-658.3
title: 'Explainer film: multi-region, cells across buckets'
status: To Do
assignee: []
created_date: '2026-11-19 15:28'
labels:
  - explainer
dependencies: []
references:
  - TASK-633
documentation:
  - docs/operating-a-host.md
parent_task_id: TASK-658
type: docs
ordinal: 1041000
---

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A 45-75 s film shows a bucket per region, hosts finding buckets by number, and forks across buckets, marking what TASK-633 has not landed
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
