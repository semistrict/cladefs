---
id: TASK-411.11
title: >-
  Old cell KEK generations stay in the cell object after rotation: dropping one
  needs writers still adding DEKs under it fenced first
status: To Do
assignee: []
created_date: '2026-11-14 23:22'
labels: []
dependencies:
  - TASK-411.3
parent_task_id: TASK-411
priority: low
type: task
ordinal: 809000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411.3 keeps every cell KEK generation after a rotation, as a writer may still be adding a DEK wrapped under an older one. So a leaked old cell KEK keeps unwrapping the DEKs it wrapped until those are rewrapped, and the cell object grows by a wrapped key each rotation. doc-16 notes it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A rotation can retire an old cell KEK generation once every DEK it wrapped is rewrapped and no writer can add one under it (fenced by the cell's epoch)
- [ ] #2 A test rotates, retires the old generation, and shows the old cell KEK unwraps nothing the cell object or its records hold
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
