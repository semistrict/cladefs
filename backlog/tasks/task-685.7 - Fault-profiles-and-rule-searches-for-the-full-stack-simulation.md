---
id: TASK-685.7
title: Fault profiles and rule searches for the full-stack simulation
status: To Do
assignee: []
created_date: '2026-11-20 03:33'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1073000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 7: network partitions (one way, both, shifting), clogs and loss; the bucket slow, failing, out of reach; zombie writers; takes and forced takes; SetPeers, drains, deletions, forks, versions, collector passes; disk and memory pressure; a search per rule in bucket_cell::Rules and local_cell::Rules.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each profile passes its seeds; each applicable rule has a search that finds its violation
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
