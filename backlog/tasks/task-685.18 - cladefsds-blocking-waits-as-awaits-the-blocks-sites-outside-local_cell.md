---
id: TASK-685.18
title: 'cladefsd''s blocking waits as awaits: the blocks! sites outside local_cell'
status: To Do
assignee: []
created_date: '2026-11-21 01:47'
labels:
  - async
  - simulation
dependencies: []
parent_task_id: TASK-685
priority: high
ordinal: 1122000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
cells.rs (a kept store's close waiting for the forks forking it; and the second blocks! there), server.rs (a shrink waiting for pages VM processes reach to be freed), cell_table.rs (a hold of a store waiting for its holder on this thread), upload.rs (an uploader dropped unclosed; upload::block_on), dir.rs (the claim's wait, now preceded by the async closed()): each an async wait, the blocks! gone; and a profile reaching them (resizes/shrinks, forks of kept stores) so that the simulation runs them.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
