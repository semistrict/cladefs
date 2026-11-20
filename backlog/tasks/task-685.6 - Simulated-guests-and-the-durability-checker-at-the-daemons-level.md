---
id: TASK-685.6
title: Simulated guests and the durability checker at the daemon's level
status: To Do
assignee: []
created_date: '2026-11-20 03:33'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1072000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 6: a guest per open store driving the daemon's request handling (no VM socket, no window), the local_cell::sim::World checker lifted to daemons (doc-12's rows per level); one attrition profile; seeds replay (MADSIM_TEST_CHECK_DETERMINISM and a request journal).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An attrition profile of 200 seeds passes and replays; turning a rule off makes a seed fail
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
