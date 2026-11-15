---
id: TASK-407.5
title: Measure the durable commit's cost under the head and document it in doc-7
status: Done
assignee: []
created_date: '2026-11-15 01:26'
updated_date: '2026-11-15 10:07'
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
- [x] #1 Sync loop measured on ubu1 under fxjob --bench, recorded in docs/measurements
- [x] #2 doc-7 describes the head and the rule that a key is written once
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Measured on ubu1 under fxjob --bench (durable_commits_through_the_daemon, file bucket on tmpfs at 25 ms a request, store on ext4 scratch; head 480962251 vs before 5632ac7a9, alternated): ~60 ms a round under the head against ~33 ms before, one round trip more (sync loop ~17/s from ~30/s). docs/measurements/durable-commits-under-the-head-2026-11-15.md; doc-7 (The head, What it costs) says it; doc-7 and doc-14 describe the head and keys written once (ed6f65cbe).
<!-- SECTION:FINAL_SUMMARY:END -->
