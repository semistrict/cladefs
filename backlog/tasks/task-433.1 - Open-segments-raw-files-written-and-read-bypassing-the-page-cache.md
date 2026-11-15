---
id: TASK-433.1
title: Open segments' raw files written and read bypassing the page cache
status: To Do
assignee: []
created_date: '2026-11-15 10:27'
labels: []
dependencies: []
parent_task_id: TASK-433
ordinal: 824000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Guest writes go to the open segment's raw file through the page cache and are read back from it (unsealed extents, commits copying pieces, the look-ahead, framing); durable commits sync keeping the pages (sync_cached, TASK-92.23). Write and read them with O_DIRECT; what is read back recently is in our own memory (the extent cache, held parts). The test durable_commits_keep_the_open_segments_pages_cached_on_the_host (TASK-315) becomes: none of a store's data is in the page cache.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
