---
id: TASK-685.15
title: >-
  Simulations in one process leak into each other: after about 75 seeds of
  madsim's attrition run in one process, the hosts' takes fail EIO
status: To Do
assignee: []
created_date: '2026-11-20 19:00'
labels: []
dependencies: []
parent_task_id: TASK-685
priority: medium
ordinal: 1099000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-20 running TASK-685.6's attrition over MADSIM_TEST_NUM=200 seeds in one process: from seed 76 on (seeds 1..75 before it, in order; alone it passes), host 3's every CreateCell take answers Internal Errno(EIO). What each simulation's hosts held stays: node-local values are leaked by design (node_local, 'as a static's is'), so their stores' open files, page cache frames and physical memory (physmem is the process's, TASK-685.14) outlive the simulation. Until simulations let go of what their nodes held, scripts/test.sh's madsim part runs each seed in a process of its own (CLADEFS_MADSIM_SEEDS).
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
