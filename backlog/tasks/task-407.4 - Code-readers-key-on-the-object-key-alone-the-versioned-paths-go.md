---
id: TASK-407.4
title: 'Code: readers key on the object key alone; the versioned paths go'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 01:25'
updated_date: '2026-11-15 09:44'
labels: []
dependencies: []
parent_task_id: TASK-407
priority: high
ordinal: 801000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
With keys written once (TASK-407), remove: the read by the footer over the index (TASK-79.18), Flown's Stored in in-flight dedup (TASK-92.43), the part cache matched by Stored, the read again by a tail fetched anew on a CRC failure, peers' not-as-asked (TASK-406.2). A CRC failure is corruption, an error.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Fetcher, part cache, in-flight reads and peers key on (key, part); those paths are gone and their tests replaced
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Pushed 1d91b8705 (fetcher, cache, peers, proto, Go client). macOS: clippy + workspace 2488/2488, go part, scripts part. Linux compile checked on ubu1; ubu1 workspace run pending.
<!-- SECTION:NOTES:END -->
