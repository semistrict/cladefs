---
id: TASK-685.6
title: Simulated guests and the durability checker at the daemon's level
status: Done
assignee: []
created_date: '2026-11-20 03:33'
updated_date: '2026-11-20 19:32'
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
- [x] #1 An attrition profile of 200 seeds passes and replays; turning a rule off makes a seed fail
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done (see main's 'Attrition under madsim' commit): cladefsd::guest::Guest drives the daemons' request handling in memory; tests/madsim.rs's uploaded_commits_survive_attrition passes 200 seeds (each a process) and replays (MADSIM_TEST_CHECK_DETERMINISM); attrition_needs_head_swaps finds a loss with Rules::head_swap off (seed 25). Verified: ubu1 3050/3050 + madsim + scripts + VMM + guest; Mac 2706/2706. Gaps: disk is the real filesystem, not a crashed MemDir (TASK-685.4); physmem per process (TASK-685.14); simulations leak into each other in one process (TASK-685.15).
<!-- SECTION:NOTES:END -->
