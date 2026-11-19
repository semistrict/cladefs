---
id: TASK-658.5
title: 'Explainer film: container images as bases'
status: To Do
assignee: []
created_date: '2026-11-19 15:29'
labels:
  - explainer
dependencies: []
documentation:
  - README.md
parent_task_id: TASK-658
type: docs
ordinal: 1043000
---

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A 45-75 s film shows cladefsctl import turning an image into a base, forks of it booting, and sharing what they read
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
