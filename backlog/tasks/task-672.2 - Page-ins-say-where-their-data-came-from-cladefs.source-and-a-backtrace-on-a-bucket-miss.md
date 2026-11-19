---
id: TASK-672.2
title: >-
  Page-ins say where their data came from: cladefs.source, and a backtrace on a
  bucket miss
status: To Do
assignee: []
created_date: '2026-11-19 22:23'
labels: []
dependencies: []
parent_task_id: TASK-672
ordinal: 1046000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
To find reads that missed every cache, a page-in must say what served it: the store (local), the part cache, a peer, or the bucket. A page-in is a spawned task (cache.rs spawns read_part), so a backtrace taken at the GET shows the executor, not the caller; the useful stack is the one where the request started the page-in, captured only when the page-in went to the bucket, so its cost lands on paths already waiting milliseconds.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cache.page_in spans carry cladefs.source: local, part_cache, peer or bucket
- [ ] #2 A page-in served by the bucket carries the backtrace of the thread that started it, shown on /tracez
- [ ] #3 /tracez can filter its samples to cladefs.source=bucket
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
