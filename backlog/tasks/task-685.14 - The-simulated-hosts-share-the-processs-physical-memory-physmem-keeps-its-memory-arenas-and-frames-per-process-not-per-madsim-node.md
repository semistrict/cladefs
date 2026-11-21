---
id: TASK-685.14
title: >-
  The simulated hosts share the process's physical memory: physmem keeps its
  memory, arenas and frames per process, not per madsim node
status: Done
assignee: []
created_date: '2026-11-20 17:15'
updated_date: '2026-11-21 02:17'
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

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
physmem's size and PMM node node_local, a page keeping its memory under madsim; each simulated host its own memory. Verified by the madsim profiles over 300 seeds (boots no longer race physmem's configure) and physmem's tests. Commit 385446f82.
<!-- SECTION:FINAL_SUMMARY:END -->
