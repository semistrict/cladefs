---
id: TASK-685.14
title: >-
  The simulated hosts share the process's physical memory: physmem keeps its
  memory, arenas and frames per process, not per madsim node
status: To Do
assignee: []
created_date: '2026-11-20 17:15'
labels: []
dependencies: []
parent_task_id: TASK-685
priority: medium
ordinal: 1096000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-20 booting two daemons under madsim (TASK-685.9): physmem::configure is process-wide (MEMORY_BYTES OnceLock, pmm node, arenas), so the second host's crate::pages::configure failed EBUSY. For now, under madsim, a host finding it configured configures nothing (crate::pages::configure): the hosts share one pool, and one host's memory pressure is every host's. A per-node physmem (node_local! statics there too) gives each simulated host its own memory, governor and arenas.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
