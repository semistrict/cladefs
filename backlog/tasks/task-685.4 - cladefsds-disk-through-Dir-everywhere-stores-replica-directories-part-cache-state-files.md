---
id: TASK-685.4
title: >-
  cladefsd's disk through Dir everywhere: stores, replica directories, part
  cache, state files
status: To Do
assignee: []
created_date: '2026-11-20 03:33'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1070000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 4: every file cladefsd writes goes through local_cell::Dir, so a simulated node's disk is a MemDir crashed with crash_corrupting on its kill.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No std::fs in cladefsd's serving paths outside Dir implementations; a lint refuses new ones
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
