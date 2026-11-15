---
id: TASK-443.7
title: >-
  Investigate: cold-read amplification (2-17 one-MiB GETs per 4 KiB touch, 64
  MiB of parts in memory host-wide, demand reads waiting for the cache's SSD
  write)
status: To Do
assignee: []
created_date: '2026-11-15 12:25'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: medium
ordinal: 841000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/extent_remote/src/fetch.rs read_ahead (AHEAD 16; a random read always fetches the next part; a second read in one part counts as sequential) is stacked under crates/fxdax/src/cache.rs PARTS_AHEAD 2, whose extra parts go out as demand; all share the host's 64-GET budget. RECENT keeps 64 MiB of fetched parts in memory for every tenant, so small-extent page-ins re-read and re-unpack 1 MiB each. Fetching::part hands the part to readers only after Cache::keep_part wrote it to the shared SSD, eviction's hole punches included. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 GET-range requests and fetched bytes for one fork's boot and first commands on a cold host are recorded with read-ahead on and off; and page-in latency with the part cache full beside a writer loading the same disk (bench_reads_while_parts_are_kept)
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
