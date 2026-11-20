---
id: TASK-685.2
title: 'Per-node statics: node_local! for cladefsd''s process state'
status: To Do
assignee: []
created_date: '2026-11-20 03:32'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1068000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 2: about 140 statics (cladefs 88, local_cell 35, bucket_cell 15) are shared by madsim's nodes. A node_local! static, plain built normally, one value per madsim node under cfg(madsim), dropped on the node's kill; each static converted, those per process by nature kept.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every static holding a daemon's state is node_local!; a test under cfg(madsim) shows two nodes' values apart and a killed node's dropped
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
