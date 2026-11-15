---
id: TASK-407.5
title: Measure the durable commit's cost under the head and document it in doc-7
status: To Do
assignee: []
created_date: '2026-11-15 01:26'
labels: []
dependencies: []
parent_task_id: TASK-407
priority: medium
ordinal: 802000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ubu1, sync loop at 25 ms bucket latency as TASK-371 measured (30.1/s): a durable commit now a segment PUT then a head swap. doc-7: the head, keys written once, the collector's epoch rule, moves by pass; superseded: fencing by create, pins, tombstones.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Sync loop measured on ubu1 under fxjob --bench, recorded in docs/measurements
- [ ] #2 doc-7 describes the head and the rule that a key is written once
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
