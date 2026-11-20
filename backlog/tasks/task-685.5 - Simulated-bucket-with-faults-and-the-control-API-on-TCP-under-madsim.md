---
id: TASK-685.5
title: 'Simulated bucket with faults, and the control API on TCP under madsim'
status: To Do
assignee: []
created_date: '2026-11-20 03:33'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1071000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 5: an in-process object store the nodes share, its requests delayed and failed before or after landing as the seed says, out of reach for stretches (bucket_cell::faults' model); the control API served on TCP under cfg(madsim), as madsim has no Unix sockets.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Two nodes' daemons serve the control API over madsim TCP and share the simulated bucket, its faults seeded
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
