---
id: TASK-672.2
title: >-
  Page-ins say where their data came from: cladefs.source, and a backtrace on a
  bucket miss
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-19 22:23'
updated_date: '2026-11-19 23:39'
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
- [ ] #2 A page-in served by the bucket carries the backtrace of the thread that started it, shown on /z/tracez
- [ ] #3 /z/tracez can filter its samples to cladefs.source=bucket
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. bucket_cell::trace: Source (memory < part_cache < peer < bucket), a per-context accumulator (sourced(future) -> (output, farthest source noted)), note(source); cladefs.source and cladefs.backtrace attributes; backtrace() capturing frame addresses (backtrace crate), resolved only when shown.
2. fetch.rs: flights (parts and tails) run sourced, their output carrying where they went, every reader noting it on landing, so a read that joins another's GET says bucket too; a recent part notes memory, the part cache part_cache, a peer peer, a GET or listing bucket.
3. cache.rs: a page-in sets cladefs.source on cache.page_in (local when the fetcher was not used); the bring_in that started it, woken with its page in, records the backtrace on the current span (the request's) when the source was bucket: captured only on bucket misses, from the request's own poll stack, as the page-in itself is a spawned task.
4. Tests: fetch notes each source, a joiner notes its flight's; a daemon read through the bucket shows source=bucket and a backtrace naming bring_in.
<!-- SECTION:PLAN:END -->
