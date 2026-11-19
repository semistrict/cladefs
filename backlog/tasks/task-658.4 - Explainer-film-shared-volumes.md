---
id: TASK-658.4
title: 'Explainer film: shared volumes'
status: To Do
assignee: []
created_date: '2026-11-19 15:29'
labels:
  - explainer
dependencies: []
documentation:
  - backlog/docs/doc-10 - Shared-volumes-Fxfs-volumes-over-read-only-bases.md
parent_task_id: TASK-658
type: docs
ordinal: 1042000
---

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A 45-75 s film shows one sandbox keeping a Git checkout current and others attaching it under writable layers of their own
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
