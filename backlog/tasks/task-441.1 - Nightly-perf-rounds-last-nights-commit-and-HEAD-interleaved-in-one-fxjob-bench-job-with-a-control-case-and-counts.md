---
id: TASK-441.1
title: >-
  Nightly perf rounds: last night's commit and HEAD interleaved in one fxjob
  --bench job, with a control case and counts
status: To Do
assignee: []
created_date: '2026-11-15 11:46'
labels:
  - testing
  - ubu1
  - perf
dependencies: []
parent_task_id: TASK-441
priority: high
ordinal: 831000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The measuring half of the nightly. scripts/bench-ab-host.sh already builds two sides as work and interleaves rounds in the bench slot; a single run per commit is no A/B on ubu1's disk (TASK-395.17: 320 s against 559 s were single runs). The baseline side is the commit the last night measured, its artifacts kept from that night so one side builds. The control case tells a host that moved from a commit that did.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Each round measures both sides and the control; a metric is reported moved only when it moved in the same direction past its bound in every kept round and the control did not
- [ ] #2 Counts (written bytes per guest byte by kind, daemon peak memory, device requests, bucket requests) are compared exactly beside the timings
- [ ] #3 The baseline's artifacts are last night's: a night builds one tree
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
